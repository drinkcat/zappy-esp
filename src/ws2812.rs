use core::time::Duration;

use esp_idf_svc::hal::rmt::config::{MemoryAccess, TxChannelConfig};
use esp_idf_svc::hal::rmt::encoder::simple_encoder::{
    EncoderCallback, NotEnoughSpace, SimpleEncoder, SymbolBuffer,
};
use esp_idf_svc::hal::rmt::{PinState, PulseTicks, Symbol, TxChannelDriver};
use esp_idf_svc::hal::units::Hertz;
use esp_idf_svc::sys::EspError;
use smart_leds_trait::{SmartLedsWrite, RGB8};

const RMT_RESOLUTION: Hertz = Hertz(10_000_000); // 10MHz, 1 tick = 0.1us

// WS2812 timing
const T0H: Duration = Duration::from_nanos(350);
const T0L: Duration = Duration::from_nanos(800);
const T1H: Duration = Duration::from_nanos(700);
const T1L: Duration = Duration::from_nanos(600);
const TRESET: Duration = Duration::from_micros(281);

struct LedEncoder;

impl EncoderCallback for LedEncoder {
    type Item = RGB8;

    fn encode(&mut self, input: &[RGB8], buf: &mut SymbolBuffer<'_>) -> Result<(), NotEnoughSpace> {
        let zero  = Symbol::new_with(RMT_RESOLUTION, PinState::High, T0H, PinState::Low, T0L).unwrap();
        let one   = Symbol::new_with(RMT_RESOLUTION, PinState::High, T1H, PinState::Low, T1L).unwrap();
        let reset = Symbol::new_half_split(RMT_RESOLUTION, PinState::Low, PinState::Low, TRESET).unwrap();

        for &c in input {
            let mut symbols = vec![reset];
            // WS2812 order: GRB
            for byte in [c.g, c.r, c.b] {
                for i in 0..8 {
                    symbols.push(if (byte >> (7 - i)) & 1 == 1 { one } else { zero });
                }
            }
            buf.write_all(&symbols)?;
        }

        // Trailing low to latch
        let max_dur = PulseTicks::max().duration(RMT_RESOLUTION) * 2;
        let tail: Vec<Symbol> = Symbol::new_half_split(RMT_RESOLUTION, PinState::Low, PinState::Low, max_dur)
            .unwrap()
            .repeat_for(RMT_RESOLUTION, Duration::from_millis(1))
            .collect();
        buf.write_all(&tail)?;

        Ok(())
    }
}

pub struct Ws2812<'d> {
    channel: TxChannelDriver<'d>,
    encoder: SimpleEncoder<LedEncoder>,
}

impl<'d> Ws2812<'d> {
    pub fn new(pin: impl esp_idf_svc::hal::gpio::OutputPin + 'd) -> Result<Self, EspError> {
        let channel = TxChannelDriver::new(
            pin,
            &TxChannelConfig {
                resolution: RMT_RESOLUTION,
                memory_access: MemoryAccess::Indirect { memory_block_symbols: 64 },
                ..Default::default()
            },
        )?;
        let encoder = SimpleEncoder::with_config(LedEncoder, &Default::default())?;
        Ok(Self { channel, encoder })
    }
}

impl SmartLedsWrite for Ws2812<'_> {
    type Error = EspError;
    type Color = RGB8;

    fn write<T, I>(&mut self, iterator: T) -> Result<(), EspError>
    where
        T: IntoIterator<Item = I>,
        I: Into<RGB8>,
    {
        let data: Vec<RGB8> = iterator.into_iter().map(Into::into).collect();
        self.channel.send_and_wait(&mut self.encoder, &data, &Default::default())
    }
}
