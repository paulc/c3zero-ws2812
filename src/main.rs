use askama::Template;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::delay::FreeRtos;
use esp_idf_svc::hal::prelude::*;
use esp_idf_svc::http;
use esp_idf_svc::http::server::{Configuration as HttpConfig, EspHttpServer};
use esp_idf_svc::nvs::{
    EspDefaultNvs, EspDefaultNvsPartition, EspNvs, EspNvsPartition, NvsDefault,
};
use esp_idf_svc::wifi::{
    AccessPointConfiguration, AccessPointInfo, AuthMethod, Configuration, EspWifi,
};
use std::str::FromStr;
//use esp_idf_svc::hal::spi;
//use esp_idf_svc::hal::gpio;
//use smart_leds::SmartLedsWrite;
//use ws2812_spi::Ws2812;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
//use std::net::Ipv4Addr;

#[derive(Serialize, Deserialize, Debug)]
struct WifiConfig {
    ssid: heapless::String<32>,
    password: heapless::String<64>,
}

impl WifiConfig {
    fn _new(ssid: &str, password: &str) -> anyhow::Result<Self> {
        Ok(WifiConfig {
            ssid: ssid
                .try_into()
                .map_err(|_| anyhow::anyhow!("Failed to create SSID"))?,
            password: password
                .try_into()
                .map_err(|_| anyhow::anyhow!("Failed to create PW"))?,
        })
    }
}

#[derive(Serialize, Deserialize, Debug)]
struct KnownAPs(Vec<heapless::String<32>>);

static WIFI_SCAN: Mutex<Vec<AccessPointInfo>> = Mutex::new(Vec::new());
static KNOWN_APS: Mutex<KnownAPs> = Mutex::new(KnownAPs(Vec::new()));
// XXX Need static NVS

#[derive(askama::Template)]
#[template(path = "../templates/config_page.html")]
struct ConfigPage<'a> {
    visible: Vec<(&'a str, u8, i8, &'a str)>,
    aps: Vec<&'a str>,
}

fn main() -> anyhow::Result<()> {
    // It is necessary to call this function once. Otherwise some patches to the runtime
    // implemented by esp-idf-sys might not link properly. See https://github.com/esp-rs/esp-idf-template/issues/71
    esp_idf_svc::sys::link_patches();

    // Bind the log crate to the ESP Logging facilities
    esp_idf_svc::log::EspLogger::initialize_default();
    log::info!("Starting...");

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;

    // Non Volatile Storage
    let nvs_default_partition: EspNvsPartition<NvsDefault> = EspDefaultNvsPartition::take()?;
    let nvs = EspDefaultNvs::new(nvs_default_partition.clone(), "ap-credentials", true)?;

    update_known_aps(&nvs)?;

    //let c = WifiConfig::new("CR-GUEST", "caribou-gnu")?;
    //save_wifi_config(&mut nvs, &c)?;
    //let c = WifiConfig::new("TEST", "test")?;
    //save_wifi_config(&mut nvs, &c)?;

    let mut wifi = EspWifi::new(
        peripherals.modem,
        sys_loop.clone(),
        Some(nvs_default_partition),
    )?;

    // Start WiFi initially with default config for scan
    let config = Configuration::Client(esp_idf_svc::wifi::ClientConfiguration {
        ..Default::default()
    });
    wifi.set_configuration(&config)?;
    wifi.start()?;

    // WiFi Scan
    wifi_scan(&mut wifi)?;

    // Check if WiFi config is stored
    let mut wifi_config: Option<WifiConfig> = None;
    for config in find_wifi_config(&nvs)? {
        log::info!("Trying network: {}", config.ssid);
        match connect_wifi(&mut wifi, &config, 10000) {
            Ok(true) => {
                log::info!("Connected to Wifi: {}", config.ssid);
                wifi_config = Some(config);
                break;
            }
            Ok(false) => {
                log::info!("Failed to connect to Wifi: {}", config.ssid);
            }
            Err(e) => {
                log::info!("Wifi Error: {} [{}]", config.ssid, e);
            }
        }
    }

    let _server = if let Some(config) = wifi_config {
        log::info!("Connected to SSID: {}", config.ssid);
        start_http_server()?
    } else {
        log::info!("No valid config found - starting AP");
        start_access_point(&mut wifi)?;
        start_http_server()?
    };

    let mut i = 0;
    loop {
        FreeRtos::delay_ms(1000); // Delay for 100 milliseconds
        log::info!("Tick: {}", i);
        i += 1;
    }
}

// We can only use 15 byte string as NVS key
fn hash_ssid(ssid: &str) -> heapless::String<15> {
    const FNV_OFFSET: u64 = 14695981039346656037;
    const FNV_PRIME: u64 = 1099511628211;
    const HEX_CHARS: &[u8; 16] = b"0123456789abcdef";

    // FNV1 64bit hash
    let mut hash = FNV_OFFSET;
    ssid.as_bytes().iter().for_each(|b| {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    });

    // Convert to hex - only use bottom 60bits
    let mut buffer: heapless::Vec<u8, 15> = heapless::Vec::new();
    for _ in 0..15 {
        let nibble = (hash & 0xF) as usize;
        // Buffer is 16 bytes long so dont need to check
        unsafe { buffer.push_unchecked(HEX_CHARS[nibble]) };
        hash >>= 4;
    }
    // We know this is valid UTF8
    heapless::String::from_utf8(buffer).unwrap()
}

fn _test_nvs(nvs: &mut EspNvs<NvsDefault>, key: &str, value: &[u8]) -> anyhow::Result<()> {
    log::info!("Setting NVS: {}", key);
    nvs.set_raw(key, value)?;

    log::info!("Getting NVS: {}", key);
    let mut data = [0_u8; 1024];
    if let Ok(Some(data)) = nvs.get_raw(key, &mut data) {
        log::info!("Found Key: {} len={} data={:?}", key, data.len(), data);
    }

    log::info!("Removing Key: {}", key);
    nvs.remove(key)?;
    Ok(())
}

fn _save_wifi_config(nvs: &mut EspNvs<NvsDefault>, c: &WifiConfig) -> anyhow::Result<()> {
    // Hash SSID in case it us >16 bytes (NVS key limit)
    let k = hash_ssid(c.ssid.as_str());
    let v = serde_json::to_vec(&c)?;
    log::info!("Setting NVS Config: {} [{}]", c.ssid, k.as_str());
    nvs.set_raw(k.as_str(), v.as_slice())
        .map_err(|e| anyhow::anyhow!("Error setting NVS config: {} [{}]", c.ssid, e))?;

    // Add to KNOWN_APS key
    let mut known_aps: KnownAPs = KnownAPs(Vec::new());
    let mut data = [0_u8; 256];
    if let Ok(Some(data)) = nvs.get_raw("KNOWN_APS", &mut data) {
        known_aps = serde_json::from_slice(data)?;
    }
    log::info!(
        ">>> KNOWN_APS: {:?} {}",
        known_aps,
        known_aps.0.contains(&c.ssid)
    );
    // Update existing value and save back to NVS
    if !known_aps.0.contains(&c.ssid) {
        known_aps.0.push(c.ssid.clone());
        let known_aps = serde_json::to_vec(&known_aps)?;
        nvs.set_raw("KNOWN_APS", known_aps.as_slice())
            .map_err(|e| anyhow::anyhow!("Error updating KNOWN_APS: [{}]", e))?;
    }

    // Updatae KNOWN_APS static
    let mut aps = KNOWN_APS.lock().unwrap();
    *aps = known_aps;
    Ok(())
}

fn _delete_wifi_config(nvs: &mut EspNvs<NvsDefault>, ssid: &str) -> anyhow::Result<()> {
    log::info!("Deleting SSID: {}", ssid);
    let k = hash_ssid(ssid);
    nvs.remove(k.as_str())?;

    // Remove from KNOWN_APS key
    let mut known_aps: KnownAPs = KnownAPs(Vec::new());
    let mut data = [0_u8; 256];
    if let Ok(Some(data)) = nvs.get_raw("KNOWN_APS", &mut data) {
        known_aps = serde_json::from_slice(data)?;
    }

    // Update existing value and save back to NVS
    if let Some(index) = known_aps.0.iter().position(|x| x == ssid) {
        known_aps.0.remove(index); // Remove the item at the found index
        let known_aps = serde_json::to_vec(&known_aps)?;
        nvs.set_raw("KNOWN_APS", known_aps.as_slice())
            .map_err(|e| anyhow::anyhow!("Error updating KNOWN_APS: [{}]", e))?;
    }
    log::info!("Updating KNOWN_APS: {:?}", known_aps);

    // Update KNOWN_APS static
    let mut aps = KNOWN_APS.lock().unwrap();
    *aps = known_aps;
    Ok(())
}

fn update_known_aps(nvs: &EspNvs<NvsDefault>) -> anyhow::Result<()> {
    let mut known_aps: KnownAPs = KnownAPs(Vec::new());
    let mut data = [0_u8; 256];
    if let Ok(Some(data)) = nvs.get_raw("KNOWN_APS", &mut data) {
        known_aps = serde_json::from_slice(data)?;
    }
    log::info!("Updating KNOWN_APS >> {:?}", known_aps);
    let mut aps = KNOWN_APS.lock().unwrap();
    *aps = known_aps;
    Ok(())
}

fn wifi_scan(wifi: &mut EspWifi) -> anyhow::Result<()> {
    log::info!("Starting WiFi scan...");
    // Note that scan will disable WiFi connection
    let scan = wifi
        .scan()?
        .into_iter()
        .inspect(|ap| {
            log::info!(
                "SSID: {:?}, Channel: {}, RSSI: {}, Auth: {:?}",
                ap.ssid,
                ap.channel,
                ap.signal_strength,
                ap.auth_method,
            )
        })
        .collect::<Vec<_>>();
    let mut aps = WIFI_SCAN.lock().unwrap();
    *aps = scan;
    Ok(())
}

fn find_wifi_config(nvs: &EspNvs<NvsDefault>) -> anyhow::Result<Vec<WifiConfig>> {
    let aps = WIFI_SCAN.lock().unwrap();
    let mut out = Vec::new();
    let mut seen = Vec::new(); // We can see same SSID on multiple bands
    for ap in aps.iter() {
        if !seen.contains(&ap.ssid.as_str()) {
            // Check if we have configuration in NVS (using hashed SSID)
            let mut data = [0_u8; 64];
            if let Ok(Some(data)) = nvs.get_raw(hash_ssid(ap.ssid.as_str()).as_str(), &mut data) {
                let config: WifiConfig = serde_json::from_slice(data)?;
                log::info!("Found Wifi Config: {}", ap.ssid);
                out.push(config);
            }
            seen.push(ap.ssid.as_str());
        }
    }
    Ok(out)
}

fn connect_wifi(wifi: &mut EspWifi, config: &WifiConfig, timeout_ms: u32) -> anyhow::Result<bool> {
    const SLEEP_MS: u32 = 500;
    let sta_config = Configuration::Client(esp_idf_svc::wifi::ClientConfiguration {
        ssid: config.ssid.clone(),
        password: config.password.clone(),
        ..Default::default()
    });

    wifi.set_configuration(&sta_config)?;
    wifi.start()?;
    wifi.connect()?;

    let mut timer = 0;
    loop {
        match wifi.is_up()? {
            true => break,
            false => {
                log::info!(
                    "Connecting: {} [{}ms] {}",
                    config.ssid,
                    timer,
                    match wifi.is_connected()? {
                        true => "<connected>",
                        false => "",
                    }
                );
                FreeRtos::delay_ms(SLEEP_MS);
                timer += SLEEP_MS;
                if timer >= timeout_ms {
                    wifi.stop()?;
                    return Ok(false);
                }
            }
        }
    }
    log::info!(
        "Connected:  {} {:?}",
        config.ssid,
        wifi.sta_netif().get_ip_info()?
    );
    Ok(true)
}

fn start_access_point(wifi: &mut EspWifi) -> anyhow::Result<()> {
    let ssid: heapless::String<32> =
        heapless::String::from_str("ESP32C3-AP").map_err(|_| anyhow::anyhow!("SSID too long"))?;
    let password: heapless::String<64> =
        heapless::String::from_str("password").map_err(|_| anyhow::anyhow!("PW too long"))?;

    let ap_config = AccessPointConfiguration {
        ssid,
        password,
        channel: 1,
        auth_method: AuthMethod::WPA2Personal,
        ..Default::default()
    };

    wifi.set_configuration(&Configuration::AccessPoint(ap_config))?;
    wifi.start()?;

    println!("Access Point started. Connect to ESP32C3-AP with password 'password'");

    // Here you would implement a web server to capture the WiFi credentials
    // and save them using `save_wifi_config`

    Ok(())
}

fn start_http_server<'a>() -> anyhow::Result<EspHttpServer<'a>> {
    let config: HttpConfig = HttpConfig {
        uri_match_wildcard: true,
        ..Default::default()
    };
    let mut server = EspHttpServer::new(&config)?;

    server.fn_handler("/hello", http::Method::Get, |req| {
        let mut response = req.into_ok_response()?;
        response.write("Hello from ESP32-C3!".as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    server.fn_handler("/", http::Method::Get, |req| {
        let aps = KNOWN_APS.lock().unwrap();
        let visible = WIFI_SCAN.lock().unwrap();
        let visible = visible
            .iter()
            .map(|ap| {
                (
                    ap.ssid.as_str(),
                    ap.channel,
                    ap.signal_strength,
                    match ap.auth_method {
                        Some(_) => "Some",
                        None => "None",
                    },
                )
            })
            .collect::<Vec<_>>();
        let config_page = ConfigPage {
            visible,
            aps: aps.0.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        };
        let mut response = req.into_ok_response()?;
        let html = config_page.render()?;
        response.write(html.as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    // Handle deleting an AP
    server.fn_handler("/delete/*", http::Method::Get, |req| {
        let _ssid = req.uri().split('/').next_back().unwrap_or("");
        let mut response = req.into_ok_response()?;
        response.write("SSID Removed".as_bytes())?;
        Ok::<(), anyhow::Error>(())
    })?;

    log::info!("Web server started");

    Ok(server)
}

/*
fn start_led_display() -> anyhow::Result<()> {
    let peripherals = Peripherals::take().unwrap();
    let spi = peripherals.spi2;
    let sclk = peripherals.pins.gpio6;
    let mosi = peripherals.pins.gpio7;

    let spi_driver = spi::SpiDriver::new(
        spi,
        sclk,
        mosi,
        Option::<gpio::Gpio0>::None,
        &spi::SpiConfig::new().baudrate(3.MHz().into()),
    )?;

    let mut ws2812 = Ws2812::new(spi_driver);

    let colors = [smart_leds::RGB8::new(255, 0, 0); 64]; // Red color for all LEDs
    ws2812.write(colors.iter().cloned())?;

    println!("LED display started");

    Ok(())
}
*/
