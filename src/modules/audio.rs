//! 系统声音峰值 —— WASAPI `IAudioMeterInformation`
//!
//! 只读取默认输出设备的**音量峰值**（一个 0.0..1.0 的标量），
//! 不打开回环捕获流、不取任何音频样本、不写文件、不联网。
//! 音量幅度无法还原声音内容，与项目「零收集 · 零上传」的隐私模型一致。
//!
//! 采样放在独立 MTA 线程：COM 初始化、设备枚举、峰值查询都不占用 UI 线程，
//! 结果通过原子量交给出帧逻辑读取；无音频设备 / 设备切换时自动重试，永不阻塞界面。

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
use windows::Win32::Media::Audio::{eMultimedia, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};

/// f32 位模式存峰值（0..1），Relaxed 足够：UI 只把它当作视觉能量输入
static PEAK: AtomicU32 = AtomicU32::new(0);
/// 未平滑的瞬时峰值：图标用它做「立刻顶上」的快速反应
static RAW: AtomicU32 = AtomicU32::new(0);

/// 当前平滑后的输出峰值 0.0..1.0；设备不可用时恒为 0
#[inline]
pub fn peak() -> f32 {
    f32::from_bits(PEAK.load(Ordering::Relaxed))
}

/// 当前瞬时峰值（未平滑）0.0..1.0
#[inline]
pub fn raw() -> f32 {
    f32::from_bits(RAW.load(Ordering::Relaxed))
}

/// 启动采样线程（进程生命周期内只调用一次）
pub fn spawn() {
    std::thread::Builder::new()
        .name("winisland-audio-meter".into())
        .spawn(sample_loop)
        .ok();
}

/// 连接默认渲染设备并取到峰值计量接口
unsafe fn open_meter() -> Option<IAudioMeterInformation> {
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
    let device = enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia).ok()?;
    let meter: IAudioMeterInformation = device.Activate(CLSCTX_ALL, None).ok()?;
    Some(meter)
}

fn sample_loop() {
    unsafe {
        // RPC_E_CHANGED_MODE / S_FALSE 都无妨：本线程独占使用 MTA
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    loop {
        let meter = unsafe { open_meter() };
        let Some(meter) = meter else {
            // 没有默认输出设备（远程桌面 / 禁用音频）时低频重试，不占 CPU
            PEAK.store(0.0f32.to_bits(), Ordering::Relaxed);
            RAW.store(0.0f32.to_bits(), Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(1500));
            continue;
        };

        let mut smooth = 0.0f32;
        // 连接成功后持续采样；设备被拔掉 / 默认设备切换时 GetPeakValue 会报错，
        // 连续失败即退出内层循环重新打开设备（静音时的正常 0 值不算失败）
        let mut fails = 0u32;
        loop {
            match unsafe { meter.GetPeakValue() } {
                Ok(v) => {
                    fails = 0;
                    let raw = v.clamp(0.0, 1.0);
                    RAW.store(raw.to_bits(), Ordering::Relaxed);
                    // 平滑值只用于「是否活跃」判断：起音快、回落也快，
                    // 停播后不残留（图标本身的波形响应直接读 raw，逐帧跟手）
                    let k = if raw > smooth { 0.75 } else { 0.35 };
                    smooth += (raw - smooth) * k;
                    if smooth < 0.002 {
                        smooth = 0.0;
                    }
                    PEAK.store(smooth.to_bits(), Ordering::Relaxed);
                }
                Err(_) => {
                    fails += 1;
                    PEAK.store(0.0f32.to_bits(), Ordering::Relaxed);
                    RAW.store(0.0f32.to_bits(), Ordering::Relaxed);
                    if fails > 10 {
                        break;
                    }
                }
            }
            // 16ms 采样：声音起落几乎与画面同帧，避免「慢半拍」
            std::thread::sleep(Duration::from_millis(16));
        }
    }
}
