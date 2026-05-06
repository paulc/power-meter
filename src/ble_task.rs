use defmt::{error, info, warn};
use embassy_executor::Spawner;
use embassy_futures::select::select;
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, channel::Sender};
use embassy_time::{Duration, Timer};
use esp_hal::rng::Rng;
use esp_radio::ble::controller::BleConnector;
use portable_atomic::{AtomicU16, AtomicU64, Ordering};
use static_cell::StaticCell;
use trouble_host::prelude::*;

pub static POWER_INSTANT: AtomicU64 = AtomicU64::new(0);
pub static POWER_AVG: AtomicU64 = AtomicU64::new(0);
pub static INA219_CONFIG: AtomicU16 = AtomicU16::new(0);

const DEVICE_NAME: &str = "POWER_METER";

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
    pub config: u16,
}

const CONNECTIONS_MAX: usize = 1;
const L2CAP_CHANNELS_MAX: usize = 2; // Signal + att
const ADV_SETS_MAX: usize = 1;

#[embassy_executor::task]
pub async fn ble_task(
    spawner: Spawner,
    bluetooth: esp_hal::peripherals::BT<'static>,
    config_tx: &'static Sender<'static, NoopRawMutex, u16, 1>,
) {
    // Initialise BT
    let connector = defmt::unwrap!(BleConnector::new(bluetooth, Default::default()));
    let controller: ExternalController<_, 1> = ExternalController::new(connector);

    // Use a random address to avoid caching
    let mut address = [0u8; 6];
    Rng::new().read(&mut address);
    info!("Address: {}", to_hex(&address));

    let address: Address = Address::random(address);

    static RESOURCES: StaticCell<
        HostResources<
            ExternalController<BleConnector, 1>,
            DefaultPacketPool,
            CONNECTIONS_MAX,
            L2CAP_CHANNELS_MAX,
            ADV_SETS_MAX,
        >,
    > = StaticCell::new();
    let resources = RESOURCES.init(HostResources::new());

    static STACK: StaticCell<Stack<ExternalController<BleConnector, 1>, DefaultPacketPool>> =
        StaticCell::new();
    let host = trouble_host::new(controller, resources)
        .set_random_address(address)
        .build();
    let stack = STACK.init(host);
    let runner = stack.runner();
    let mut peripheral = stack.peripheral();

    info!("Starting Advertising and GATT service");
    let server = defmt::unwrap!(PowerMeterServer::new_with_config(GapConfig::Peripheral(
        PeripheralConfig {
            name: DEVICE_NAME,
            appearance: &appearance::sensor::GENERIC_SENSOR,
        }
    )));

    // Spawn BLE task
    spawner.spawn(defmt::unwrap!(ble_runner_task(runner)));

    loop {
        match advertise(DEVICE_NAME, &mut peripheral, &server).await {
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
    config_tx: &'static Sender<'static, NoopRawMutex, u16, 1>,
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
                        } else if event.handle() == server.power_meter_service.config.handle() {
                            let config = INA219_CONFIG.load(Ordering::Relaxed);
                            info!("[gatt] Read config -> 0x{:x}", config);
                            if let Err(_e) = server.set(&server.power_meter_service.config, &config)
                            {
                                error!("[gatt] Update power_meter_service.config")
                            }
                        } else {
                            info!("[gatt] Read Event: {:?}", event.handle());
                        }
                    }
                    GattEvent::Write(event) => {
                        if event.handle() == server.power_meter_service.config.handle() {
                            let data = event.data();
                            if data.len() != 2 {
                                error!("[gatt] Invalid write: server.power_meter_service.config");
                            } else {
                                info!("[gatt] Write power_meter_service.config -> {:x}", data);
                                let mut bytes = [0_u8; 2];
                                bytes.copy_from_slice(data);
                                let config = u16::from_le_bytes(bytes);
                                config_tx.send(config).await;
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
        Timer::after(Duration::from_secs(5)).await;
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
            AdStructure::CompleteServiceUuids16(&[[0x01, 0x18]]),
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

const HEX_CHARS: [u8; 16] = *b"0123456789abcdef";

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
