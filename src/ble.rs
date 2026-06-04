use defmt::{error, info, warn};
use embassy_executor::Spawner;
use embassy_futures::select::select;
use embassy_sync::channel::Sender;
use embassy_time::{Duration, Timer};
use esp_hal::rng::Rng;
use esp_radio::ble::controller::BleConnector;
use esp_sync::RawMutex;
use portable_atomic::{AtomicU16, AtomicU32, AtomicU64, Ordering};
use static_cell::StaticCell;
use trouble_host::prelude::*;

pub static POWER_INSTANT: AtomicU64 = AtomicU64::new(0);
pub static POWER_AVG: AtomicU64 = AtomicU64::new(0);
pub static INA219_CONFIG: AtomicU16 = AtomicU16::new(0);
pub static NOTIFY_INTERVAL: AtomicU32 = AtomicU32::new(5);

const DEVICE: &str = "INA219_BT";

// GATT Server
#[gatt_server]
struct PowerMeterServer {
    power_meter_service: PowerMeterService,
}

/// GATT Service table definition
#[gatt_service(uuid = "00000001-9b04-4347-98ff-57e8f7803509")]
pub struct PowerMeterService {
    #[characteristic(uuid = "00000002-9b04-4347-98ff-57e8f7803509", read)]
    pub power_instant: [u8; 8],
    #[characteristic(uuid = "00000003-9b04-4347-98ff-57e8f7803509", read, notify)]
    pub power_average: [u8; 8],
    #[characteristic(uuid = "00000004-9b04-4347-98ff-57e8f7803509", read, write)]
    pub ina219_config: u16,
    #[characteristic(uuid = "00000005-9b04-4347-98ff-57e8f7803509", read, write)]
    pub notify_interval: u32,
}

const CONNECTIONS_MAX: usize = 1;
const L2CAP_CHANNELS_MAX: usize = 2; // Signal + att
const HEX_CHARS: [u8; 16] = *b"0123456789abcdef";

#[embassy_executor::task]
pub async fn ble_task(
    spawner: Spawner,
    bluetooth: esp_hal::peripherals::BT<'static>,
    config_tx: &'static Sender<'static, RawMutex, u16, 1>,
) {
    // Initialise BT
    let transport = BleConnector::new(bluetooth, Default::default()).unwrap();
    let ble_controller = ExternalController::<_, 1>::new(transport);

    // Use a random address to avoid caching
    let mut rnd = [0u8; 6];
    Rng::new().read(&mut rnd);
    info!("Address (Rnd): {}", to_hex(&rnd));

    // Add address bytes to name
    static DEVICE_NAME: StaticCell<heapless::String<16>> = StaticCell::new();
    let device_name = DEVICE_NAME.init(heapless::String::new());
    device_name.push_str(DEVICE).ok();
    device_name.push_str("_").ok();
    for b in &rnd[(rnd.len() - 2)..] {
        device_name
            .push(HEX_CHARS[(b >> 4) as usize] as char)
            .and_then(|_| device_name.push(HEX_CHARS[(b & 0x0F) as usize] as char))
            .ok();
    }

    info!("Device Name: {}", device_name);

    // Generate BLE address
    let address: Address = Address::random(rnd);

    static RESOURCES: StaticCell<
        HostResources<DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX>,
    > = StaticCell::new();
    let resources = RESOURCES.init(HostResources::new());

    static STACK: StaticCell<Stack<ExternalController<BleConnector, 1>, DefaultPacketPool>> =
        StaticCell::new();
    let stack =
        STACK.init(trouble_host::new(ble_controller, resources).set_random_address(address));

    let Host {
        mut peripheral,
        runner,
        ..
    } = stack.build();

    spawner.spawn(defmt::unwrap!(ble_runner_task(runner)));

    info!("Starting Advertising and GATT service");
    let server = defmt::unwrap!(PowerMeterServer::new_with_config(GapConfig::Peripheral(
        PeripheralConfig {
            name: device_name.as_str(),
            appearance: &appearance::sensor::GENERIC_SENSOR,
        }
    )));

    loop {
        match advertise(device_name.as_str(), &mut peripheral, &server).await {
            Ok(conn) => {
                info!("Connection");
                // Create GATT & Notify tasks
                let gatt_task = gatt_events_task(&server, &conn, config_tx);
                let notify_task = notify_task(&server, &conn, stack);
                // Wait for task to exit
                select(gatt_task, notify_task).await;
            }
            Err(e) => {
                panic!("[adv] error: {:?}", e);
            }
        }
    }
}

#[embassy_executor::task]
async fn ble_runner_task(
    mut runner: Runner<'static, ExternalController<BleConnector<'static>, 1>, DefaultPacketPool>,
) {
    info!("Starting BLE task");
    runner.run().await.expect("Error starting BLE task");
}

/// Handle GATT events
async fn gatt_events_task<P: PacketPool>(
    server: &PowerMeterServer<'_>,
    conn: &GattConnection<'_, '_, P>,
    config_tx: &'static Sender<'static, RawMutex, u16, 1>,
) -> Result<(), Error> {
    let reason = loop {
        match conn.next().await {
            GattConnectionEvent::Disconnected { reason } => break reason,
            GattConnectionEvent::Gatt { event } => {
                match &event {
                    GattEvent::Read(event) => {
                        if event.handle() == server.power_meter_service.power_instant.handle() {
                            let p_instant = POWER_INSTANT.load(Ordering::Relaxed).to_le_bytes();
                            info!("[gatt] Read power_instant -> {}", to_hex(&p_instant));
                            if let Err(_e) =
                                server.set(&server.power_meter_service.power_instant, &p_instant)
                            {
                                error!("[gatt] Update power_meter_service.power_instant")
                            }
                        } else if event.handle()
                            == server.power_meter_service.power_average.handle()
                        {
                            let p_avg = POWER_AVG.load(Ordering::Relaxed).to_le_bytes();
                            info!("[gatt] Read power_average -> {}", to_hex(&p_avg));
                            if let Err(_e) =
                                server.set(&server.power_meter_service.power_average, &p_avg)
                            {
                                error!("[gatt] Update power_meter_service.power_average")
                            }
                        } else if event.handle()
                            == server.power_meter_service.ina219_config.handle()
                        {
                            let config = INA219_CONFIG.load(Ordering::Relaxed);
                            info!("[gatt] Read ina219_config -> 0x{:x}", config);
                            if let Err(_e) =
                                server.set(&server.power_meter_service.ina219_config, &config)
                            {
                                error!("[gatt] Update power_meter_service.ina219_config")
                            }
                        } else if event.handle()
                            == server.power_meter_service.notify_interval.handle()
                        {
                            let interval = NOTIFY_INTERVAL.load(Ordering::Relaxed);
                            info!("[gatt] Read notify_interval -> {}", interval);
                            if let Err(_e) =
                                server.set(&server.power_meter_service.notify_interval, &interval)
                            {
                                error!("[gatt] Update power_meter_service.ina219_config")
                            }
                        } else {
                            info!("[gatt] Unknown Read Event: {:?}", event.handle());
                        }
                    }
                    GattEvent::Write(event) => {
                        if event.handle() == server.power_meter_service.ina219_config.handle() {
                            let data = event.data();
                            if data.len() != 2 {
                                error!("[gatt] Invalid write: server.power_meter_service.ina219_config");
                            } else {
                                info!(
                                    "[gatt] Write power_meter_service.ina219_config -> {:x}",
                                    data
                                );
                                let mut bytes = [0_u8; 2];
                                bytes.copy_from_slice(data);
                                let config = u16::from_le_bytes(bytes);
                                config_tx.send(config).await;
                            }
                        } else if event.handle()
                            == server.power_meter_service.notify_interval.handle()
                        {
                            let data = event.data();
                            if data.len() != 4 {
                                error!("[gatt] Invalid write: server.power_meter_service.notify_interval");
                            } else {
                                let mut bytes = [0_u8; 4];
                                bytes.copy_from_slice(data);
                                let interval = u32::from_le_bytes(bytes);
                                if interval >= 1 {
                                    info!(
                                        "[gatt] Write power_meter_service.notify_interval-> {}",
                                        interval
                                    );
                                    NOTIFY_INTERVAL.store(interval, Ordering::Relaxed);
                                } else {
                                    error!(
                                        "[gatt] Invalid power_meter_service.notify_interval-> {}",
                                        interval
                                    );
                                }
                            }
                        } else {
                            info!(
                                "[gatt] Unhandled Write Event: {} {:?}",
                                event.handle(),
                                event.data()
                            );
                        }
                    }
                    _ => {}
                };
                // This step is also performed at drop(), but writing it explicitly is necessary
                // in order to ensure reply is sent.
                match event.accept() {
                    Ok(reply) => reply.send().await,
                    // Err(e) => warn!("[gatt] error sending response: {:?}", e),
                    Err(_e) => warn!("[gatt] error sending response:"),
                };
            }
            _ => {} // ignore other Gatt Connection Events
        }
    };
    info!("[gatt] disconnected: {:?}", reason);
    Ok(())
}

async fn notify_task<C: Controller, P: PacketPool>(
    server: &PowerMeterServer<'_>,
    conn: &GattConnection<'_, '_, P>,
    _stack: &Stack<'_, C, P>,
) {
    let characteristic = server.power_meter_service.power_average;
    loop {
        let p_avg = POWER_AVG.load(Ordering::Relaxed).to_le_bytes();
        if characteristic.notify(conn, &p_avg).await.is_err() {
            error!("Notify Error: server.power_meter_service.power_average");
            break;
        } else {
            info!(
                "Notify: server.power_meter_service.power_average -> {}",
                to_hex(&p_avg)
            );
        }
        Timer::after(Duration::from_secs(
            NOTIFY_INTERVAL.load(Ordering::Relaxed) as u64
        ))
        .await;
    }
}

/// Create an advertiser to use to connect to a BLE Central, and wait for it to connect.
async fn advertise<'values, 'server, C: Controller>(
    name: &'values str,
    peripheral: &mut Peripheral<'values, C, DefaultPacketPool>,
    server: &'server PowerMeterServer<'values>,
) -> Result<GattConnection<'values, 'server, DefaultPacketPool>, BleHostError<C::Error>> {
    let mut advertiser_data = [0; 31];
    let len = AdStructure::encode_slice(
        &[
            AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
            AdStructure::ServiceUuids16(&[[0x01, 0x18]]),
            AdStructure::CompleteLocalName(name.as_bytes()),
        ],
        &mut advertiser_data[..],
    )?;
    let advertiser = peripheral
        .advertise(
            &Default::default(),
            Advertisement::ConnectableScannableUndirected {
                adv_data: &advertiser_data[..len],
                scan_data: &[],
            },
        )
        .await?;
    info!("[adv] advertising");
    let conn = advertiser.accept().await?.with_attribute_server(server)?;
    info!("[adv] connection established");
    Ok(conn)
}

pub fn to_hex<const N: usize>(bytes: &[u8; N]) -> heapless::String<64> {
    let mut s = heapless::String::new();
    for &byte in bytes {
        match s
            .push(HEX_CHARS[(byte >> 4) as usize] as char)
            .and_then(|()| s.push(HEX_CHARS[(byte & 0x0F) as usize] as char))
        {
            Ok(_) => {}
            Err(_) => break, // Out of space
        }
    }
    s
}
