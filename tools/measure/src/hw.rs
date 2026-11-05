//! Opening a card with exactly the parameters a probe asked for.

use alsa::pcm::{Access, Format, Frames, HwParams, PCM};
use alsa::{Direction, ValueOr};

pub const CHANNELS: u32 = 2;

/// Open `device` as S32 stereo. Fails if the card changes any parameter,
/// because a probe must not report on a configuration it did not run.
pub fn open(
    device: &str,
    direction: Direction,
    rate: u32,
    period: usize,
    periods: u32,
) -> Result<PCM, String> {
    let e = |what: &str, err: alsa::Error| format!("{what}: {err}");
    let pcm = PCM::new(device, direction, false).map_err(|x| e("open", x))?;
    let wanted_period = period as Frames;
    let wanted_buffer = wanted_period * Frames::from(periods);
    {
        let hw = HwParams::any(&pcm).map_err(|x| e("hw params", x))?;
        hw.set_channels(CHANNELS).map_err(|x| e("channels", x))?;
        hw.set_rate(rate, ValueOr::Nearest)
            .map_err(|x| e("rate", x))?;
        hw.set_format(Format::S32LE).map_err(|x| e("format", x))?;
        hw.set_access(Access::RWInterleaved)
            .map_err(|x| e("access", x))?;
        hw.set_period_size(wanted_period, ValueOr::Nearest)
            .map_err(|x| e("period", x))?;
        hw.set_periods(periods, ValueOr::Nearest)
            .map_err(|x| e("periods", x))?;
        pcm.hw_params(&hw).map_err(|x| e("apply", x))?;
        let got_rate = hw.get_rate().map_err(|x| e("get rate", x))?;
        let got_period = hw.get_period_size().map_err(|x| e("get period", x))?;
        let got_buffer = hw.get_buffer_size().map_err(|x| e("get buffer", x))?;
        if (got_rate, got_period, got_buffer) != (rate, wanted_period, wanted_buffer) {
            return Err(format!(
                "card changed the configuration: rate {got_rate}, period {got_period}, \
                 buffer {got_buffer}"
            ));
        }
    }
    {
        let sw = pcm.sw_params_current().map_err(|x| e("sw params", x))?;
        sw.set_start_threshold(wanted_buffer)
            .map_err(|x| e("start threshold", x))?;
        sw.set_avail_min(wanted_period)
            .map_err(|x| e("avail min", x))?;
        pcm.sw_params(&sw).map_err(|x| e("apply sw", x))?;
    }
    Ok(pcm)
}
