use core::sync::atomic::{AtomicU32, Ordering};
use defmt::{error, info, warn};
use embassy_executor::Spawner;
use embassy_futures::select::select;
use embassy_time::{Duration, Timer};
use esp_hal::rng::Rng;
use esp_radio::ble::controller::BleConnector;
use static_cell::StaticCell;
use trouble_host::prelude::*;

/// Static counter that increments on each read
pub static COUNTER: AtomicU32 = AtomicU32::new(0);

const DEVICE_NAME: &'static str = "ESP32C6-COUNTER";

// GATT Server
#[gatt_server]
struct CounterServer {
    counter_service: CounterService,
}

/// GATT Service table definition
#[gatt_service(uuid = "00000001-7104-4a99-8a78-02108a60f098")]
pub struct CounterService {
    #[characteristic(uuid = "00000002-7104-4a99-8a78-02108a60f098", read, write)]
    pub counter: u32,
    #[characteristic(uuid = "00000003-7104-4a99-8a78-02108a60f098", read, write, value = [b'A';16])]
    pub name: [u8; 16],
    #[characteristic(uuid = "00000004-7104-4a99-8a78-02108a60f098", read, write, notify)]
    pub notify: u32,
    #[characteristic(uuid = "00000005-7104-4a99-8a78-02108a60f098", read, write)]
    pub c5: bool,
    #[characteristic(uuid = "00000006-7104-4a99-8a78-02108a60f098", read, write)]
    pub c6: f32,
}

const CONNECTIONS_MAX: usize = 1;
const L2CAP_CHANNELS_MAX: usize = 2; // Signal + att
const ADV_SETS_MAX: usize = 1;
#[embassy_executor::task]
pub async fn ble_task(spawner: Spawner, bluetooth: esp_hal::peripherals::BT<'static>) {
    // Initialise BT
    let connector = BleConnector::new(bluetooth, Default::default()).unwrap();
    let controller: ExternalController<_, 1> = ExternalController::new(connector);

    // Use a random address to avoid caching
    let mut address = [0u8; 6];
    Rng::new().read(&mut address);
    info!("Address: {}", to_hex(&address));

    let address: Address = Address::random(address);

    static RESOURCES: StaticCell<
        HostResources<DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX, ADV_SETS_MAX>,
    > = StaticCell::new();
    let resources = RESOURCES.init(HostResources::new());

    static STACK: StaticCell<Stack<ExternalController<BleConnector, 1>, DefaultPacketPool>> =
        StaticCell::new();
    let stack = STACK.init(trouble_host::new(controller, resources).set_random_address(address));

    let Host {
        mut peripheral,
        runner,
        ..
    } = stack.build();

    info!("Starting Advertising and GATT service");
    let server = CounterServer::new_with_config(GapConfig::Peripheral(PeripheralConfig {
        name: DEVICE_NAME,
        appearance: &appearance::sensor::GENERIC_SENSOR,
    }))
    .unwrap();

    // Spawn BLE task
    spawner.spawn(ble_runner_task(runner).expect("Spawn ble_runner_task"));

    loop {
        match advertise(DEVICE_NAME, &mut peripheral, &server).await {
            Ok(conn) => {
                info!("Connection");
                // Create GATT & Notify tasks
                let gatt_task = gatt_events_task(&server, &conn);
                let notify_task = notify_task(&server, &conn, &stack);
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
    server: &CounterServer<'_>,
    conn: &GattConnection<'_, '_, P>,
) -> Result<(), Error> {
    let reason = loop {
        match conn.next().await {
            GattConnectionEvent::Disconnected { reason } => break reason,
            GattConnectionEvent::Gatt { event } => {
                match &event {
                    GattEvent::Read(event) => {
                        info!("[gatt] Read Event: {:?}", event.handle());
                        if event.handle() == server.counter_service.counter.handle() {
                            if let Err(_e) = server.set(
                                &server.counter_service.counter,
                                &COUNTER.load(Ordering::Relaxed),
                            ) {
                                // error!("[gatt] Set server.counter_service.counter: {:?}", e)
                                error!("[gatt] Set server.counter_service.counter:")
                            }
                        }
                    }
                    GattEvent::Write(event) => {
                        info!("[gatt] Write Event: {} {:?}", event.handle(), event.data());
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
    server: &CounterServer<'_>,
    conn: &GattConnection<'_, '_, P>,
    _stack: &Stack<'_, C, P>,
) {
    let characteristic = server.counter_service.notify;
    loop {
        let v = characteristic.get(server).unwrap_or(0_u32) + 1;
        if characteristic.notify(conn, &v).await.is_err() {
            error!("Notify Error");
            break;
        }
        info!("Notify: {}", v);
        Timer::after(Duration::from_secs(5)).await;
    }
}

/// Create an advertiser to use to connect to a BLE Central, and wait for it to connect.
async fn advertise<'values, 'server, C: Controller>(
    name: &'values str,
    peripheral: &mut Peripheral<'values, C, DefaultPacketPool>,
    server: &'server CounterServer<'values>,
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
