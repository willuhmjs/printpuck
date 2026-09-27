// GPIO map for the Spotpear ESP32-S3 1.28" round touch box.
// Source: vendor board config from the xiaozhi-esp32 project
// (main/boards/spotpear/sp-esp32-s3-1.28-box/config.h), extracted from
// this exact unit's factory firmware. Same board the assistant runs on.

pub const BUILTIN_LED: u8 = 48;
pub const BOOT_BUTTON: u8 = 0;

pub const DISPLAY_WIDTH: u16 = 240;
pub const DISPLAY_HEIGHT: u16 = 240;

pub const DISPLAY_BACKLIGHT: u8 = 42; // active-low
pub const DISPLAY_SPI_SCLK: u8 = 4;
pub const DISPLAY_SPI_MOSI: u8 = 2;
pub const DISPLAY_SPI_CS: u8 = 5;
pub const DISPLAY_SPI_DC: u8 = 47;
pub const DISPLAY_SPI_RESET: u8 = 38;

pub const TOUCH_I2C_SDA: u8 = 11;
pub const TOUCH_I2C_SCL: u8 = 7;
pub const TOUCH_RST: u8 = 6;
pub const TOUCH_INT: u8 = 12;
pub const TOUCH_CST816D_ADDR: u8 = 0x15;

pub const BATTERY_ADC: u8 = 1;
pub const BATTERY_CHARGING: u8 = 41;
