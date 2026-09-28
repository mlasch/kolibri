//! kolibri on the Seeed Studio XIAO ESP32-C3: HAL setup, pin map and the
//! on-chip temperature sensor. The tasks are generic loops from [`kolibri_core`].
//!
//! SH1106 OLED on `D4`/`D5`, LED (with ~150 Ohm resistor) on `D10`.

#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_time::Duration;
use esp_backtrace as _;
use esp_bootloader_esp_idf::partitions::{
    self, DataPartitionSubType, FlashStorage, PARTITION_TABLE_MAX_LEN, PartitionType,
};
use esp_hal::{
    Async,
    gpio::{Level, Output, OutputConfig},
    i2c::master::{Config as I2cConfig, I2c},
    peripherals::{FLASH, TSENS},
    time::Rate,
    timer::timg::TimerGroup,
    tsens::{Config as TsensConfig, ConfigError, TemperatureSensor},
};
// Linked for its `critical-section` feature; see Cargo.toml.
use esp_storage as _;
use kolibri_core::{
    display::{Labels, Sh1106I2c},
    storage::Store,
    temperature::{Celsius, Source},
};

// Emits the esp-idf application descriptor the second-stage bootloader expects.
esp_bootloader_esp_idf::esp_app_desc!();

/// I2C address of the SH1106 module.
const DISPLAY_ADDRESS: u8 = kolibri_core::display::DEFAULT_ADDRESS;

/// 400 kHz pushes a full frame in ~26 ms (vs ~100 ms at the default 100 kHz).
const I2C_FREQUENCY: Rate = Rate::from_khz(400);

/// What the screen says it is running on.
const LABELS: Labels = Labels {
    board: "esp32-c3",
    source: <Sensor as Source>::LABEL,
};

/// Concrete display type; Embassy tasks cannot take `impl Trait` arguments.
type Display = Sh1106I2c<I2c<'static, Async>>;

/// Temperature source; point at another [`Source`] impl to change sensors.
type Sensor = InternalSensor;

/// The ESP32-C3's on-chip sensor. Measures the die, which runs well above ambient.
struct InternalSensor {
    sensor: TemperatureSensor<'static>,
}

impl InternalSensor {
    /// Powers the sensor up.
    fn new(peripheral: TSENS<'static>) -> Result<Self, ConfigError> {
        Ok(Self {
            sensor: TemperatureSensor::new(peripheral, TsensConfig::default())?,
        })
    }
}

impl Source for InternalSensor {
    const LABEL: &'static str = "chip";
    const WARMUP: Duration = Duration::from_micros(200);

    #[allow(clippy::unused_async_trait_impl)]
    async fn read(&mut self) -> Option<Celsius> {
        Some(Celsius::from_degrees(
            self.sensor.get_temperature().to_celsius(),
        ))
    }
}

/// Settings record capacity; room for an SSID (32) and WPA2 passphrase (63).
const SETTINGS_CAPACITY: usize = 128;

/// Increments the boot counter in the `nvs` partition and returns the new
/// total, or `None` if the record could not be read or written.
fn count_boot(flash: FLASH<'static>) -> Option<u32> {
    let mut flash = FlashStorage::new(flash);

    let mut raw = [0u8; PARTITION_TABLE_MAX_LEN];
    let table = match partitions::read_partition_table(&mut flash, &mut raw) {
        Ok(table) => table,
        Err(error) => {
            log::warn!("no partition table: {error:?}");
            return None;
        }
    };
    let entry = match table.find_partition(PartitionType::Data(DataPartitionSubType::Nvs)) {
        Ok(Some(entry)) => entry,
        Ok(None) => {
            log::warn!("no nvs partition to store settings in");
            return None;
        }
        Err(error) => {
            log::warn!("partition table unreadable: {error:?}");
            return None;
        }
    };

    let mut region = entry.as_flash_region(&mut flash);
    let nor = region.as_nor_flash().ok()?;
    let mut store = match Store::<_, SETTINGS_CAPACITY>::new(nor) {
        Ok(store) => store,
        Err(error) => {
            log::warn!("nvs partition cannot hold the settings record: {error:?}");
            return None;
        }
    };

    let mut settings = [0u8; SETTINGS_CAPACITY];
    let previous = match store.load(&mut settings) {
        Ok(Some(len)) if len >= 4 => {
            u32::from_le_bytes([settings[0], settings[1], settings[2], settings[3]])
        }
        // Nothing stored yet, or a record from before this counter existed.
        Ok(_) => 0,
        Err(error) => {
            log::warn!("settings unreadable: {error:?}");
            return None;
        }
    };

    let boots = previous.wrapping_add(1);
    match store.save(&boots.to_le_bytes()) {
        Ok(()) => Some(boots),
        Err(error) => {
            log::warn!("settings not written: {error:?}");
            None
        }
    }
}

/// Blinks the LED on `D10`.
#[embassy_executor::task]
async fn blink(led: Output<'static>) {
    kolibri_core::blink::run(led).await;
}

/// Logs a heartbeat and retunes the blink rate.
#[embassy_executor::task]
async fn heartbeat() {
    kolibri_core::heartbeat::run().await;
}

/// Samples the temperature sensor.
#[embassy_executor::task]
async fn temperature(sensor: Sensor) {
    kolibri_core::temperature::run(sensor).await;
}

/// Draws the splash and then the status screen.
#[embassy_executor::task]
async fn display(panel: Display) {
    kolibri_core::display::run(panel, LABELS).await;
}

/// Brings the chip up, starts the scheduler, and spawns the application tasks.
#[esp_hal::main]
async fn main(spawner: Spawner) {
    // Level from ESP_LOG at build time (see .cargo/config.toml).
    esp_println::logger::init_logger_from_env();

    let peripherals = esp_hal::init(esp_hal::Config::default());

    // Also installs the embassy-time driver.
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    log::info!("kolibri starting on XIAO ESP32-C3");

    match count_boot(peripherals.FLASH) {
        Some(boots) => log::info!("boot #{boots}"),
        None => log::warn!("settings storage unavailable; boot not counted"),
    }

    // D10.
    let led = Output::new(peripherals.GPIO10, Level::Low, OutputConfig::default());

    // D4/D5.
    let i2c = I2c::new(
        peripherals.I2C0,
        I2cConfig::default().with_frequency(I2C_FREQUENCY),
    )
    .expect("i2c config rejected")
    .with_sda(peripherals.GPIO6)
    .with_scl(peripherals.GPIO7)
    .into_async();

    let oled: Display = kolibri_core::display::sh1106_i2c(i2c, DISPLAY_ADDRESS);
    log::info!("display on 0x{DISPLAY_ADDRESS:02x}");

    let sensor = Sensor::new(peripherals.TSENS).expect("temperature sensor config rejected");

    // Only fails if a task's pool is already full.
    spawner.spawn(blink(led).expect("blink task pool exhausted"));
    spawner.spawn(heartbeat().expect("heartbeat task pool exhausted"));
    spawner.spawn(temperature(sensor).expect("temperature task pool exhausted"));
    spawner.spawn(display(oled).expect("display task pool exhausted"));
}
