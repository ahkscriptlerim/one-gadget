#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]   // Hides console ONLY in release build (Ctrl+C works in dev)

use rustc_hash::FxHashMap;
use std::ptr::{null, null_mut};
use std::sync::{mpsc, OnceLock};
mod config;
use std::time::{Duration, Instant};

use sysinfo::{Disks, Networks, ProcessesToUpdate, System};

use windows::core::{PCWSTR, w};
use windows::Win32::Foundation::{COLORREF, ERROR_SUCCESS, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::GdiPlus::*;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetModuleFileNameW};
use windows::Win32::System::Console::SetConsoleCtrlHandler;
use windows::Win32::System::Registry::{RegOpenKeyExW, RegSetValueExW, RegDeleteValueW, RegQueryValueExW, HKEY_CURRENT_USER, KEY_SET_VALUE, KEY_QUERY_VALUE, REG_SZ, REG_VALUE_TYPE, RegCloseKey};
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW, PdhGetFormattedCounterValue,
    PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PDH_FMT_COUNTERVALUE,
};
use windows::Win32::UI::HiDpi::{
    GetDpiForMonitor, GetDpiForSystem, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, MDT_EFFECTIVE_DPI,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::Win32::UI::Shell::{Shell_NotifyIconW, NOTIFYICONDATAW, NIM_ADD, NIM_DELETE, NIF_ICON, NIF_MESSAGE, NIF_TIP, ExtractIconExW, ShellExecuteW};
use windows::Win32::Graphics::Dxgi::*;

use windows::Win32::System::Threading::{GetCurrentThread, GetCurrentProcess, SetThreadPriority, SetProcessWorkingSetSize, THREAD_PRIORITY_BELOW_NORMAL};

const WM_USER_NEW_FRAME: u32 = WM_APP + 1;
const WM_USER_TRAY_ICON: u32 = WM_APP + 2;
const WM_USER_DPI_CHANGED: u32 = WM_APP + 3;
const WM_USER_CONFIG_RELOADED: u32 = WM_APP + 4;
const WM_DPICHANGED: u32 = 0x02E0;

const WM_POWERBROADCAST: u32 = 0x0218;
const PBT_APMSUSPEND: usize = 0x0004;
const PBT_APMRESUMEAUTOMATIC: usize = 0x0012;
const PBT_APMRESUMESUSPEND: usize = 0x0007;

static IS_SYSTEM_SUSPENDED: AtomicBool = AtomicBool::new(false);
static CONFIG_RELOAD_REQUESTED: AtomicBool = AtomicBool::new(false);

#[repr(C)]
#[allow(non_snake_case)]
struct OSVERSIONINFOEXW {
    dwOSVersionInfoSize: u32,
    dwMajorVersion: u32,
    dwMinorVersion: u32,
    dwBuildNumber: u32,
    dwPlatformId: u32,
    szCSDVersion: [u16; 128],
    wServicePackMajor: u16,
    wServicePackMinor: u16,
    wSuiteMask: u16,
    wProductType: u8,
    wReserved: u8,
}
extern "system" {
    fn RtlGetVersion(lpVersionInformation: *mut OSVERSIONINFOEXW) -> i32;
}

unsafe fn add_tray_icon(hwnd: HWND) {
    if IS_TRAY_ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    let mut icon_large: HICON = HICON(std::ptr::null_mut());
    let mut icon_small: HICON = HICON(std::ptr::null_mut());

    // 1. Try loading our embedded application icon (compiled via build.rs) at exact system tray dimensions
    let instance = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
    let cx_sm = GetSystemMetrics(SM_CXSMICON);
    let cy_sm = GetSystemMetrics(SM_CYSMICON);
    
    if let std::result::Result::Ok(handle) = LoadImageW(
        Some(HINSTANCE(instance.0)),
        PCWSTR(1 as *const u16),
        IMAGE_ICON,
        cx_sm,
        cy_sm,
        LR_DEFAULTCOLOR | LR_SHARED,
    ) {
        if !handle.is_invalid() && !handle.0.is_null() {
            icon_small = HICON(handle.0);
        }
    }

    // 2. Fallback to system DLL icon if no embedded icon is present (e.g. uncompiled dev run)
    if icon_small.0.is_null() {
        let mut os_info = OSVERSIONINFOEXW {
            dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOEXW>() as u32,
            ..std::mem::zeroed()
        };
        RtlGetVersion(&mut os_info);
        
        // Windows 11 builds are >= 22000
        if os_info.dwBuildNumber >= 22000 {
            ExtractIconExW(w!("taskmgr.exe"), 0, Some(&mut icon_large), Some(&mut icon_small), 1);
        } else {
            ExtractIconExW(w!("imageres.dll"), 144, Some(&mut icon_large), Some(&mut icon_small), 1);
        }
    }
    
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = WM_USER_TRAY_ICON;
    nid.hIcon = if !icon_small.0.is_null() { icon_small } else { icon_large };
    
    let stem = &crate::config::get_identity().exe_stem;
    for (i, c) in stem.encode_utf16().enumerate().take(127) {
        nid.szTip[i] = c;
    }
    
    if Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
        IS_TRAY_ACTIVE.store(true, Ordering::Relaxed);
    }
}

unsafe fn remove_tray_icon(hwnd: HWND) {
    if !IS_TRAY_ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    IS_TRAY_ACTIVE.store(false, Ordering::Relaxed);
}

// ============================================================================
//                          USER INIT CONFIGURATION
// ============================================================================

/// Attach directly to Explorer's WorkerW wallpaper tree (Active Wallpaper mode).
/// Note: Windows DWM disables layered window transparency for child windows, so false is required for desktop gadgets.
pub const ATTACH_TO_WORKERW: bool = false;  // true çalışmıyor

/// Keep gadget on top of all windows (Always-on-top mode).
/// Set to true to float above all windows.
/// Set to false to sit on the desktop canvas behind normal app windows.
pub const ALWAYS_ON_TOP: bool = false;

pub const TARGET_MONITOR_INDEX: usize = 0;

/// High-frequency sensor sampling interval in seconds (for EMA smoothing)
pub const POLL_INTERVAL_SECS: f32 = 0.5;
/// Reciprocal of POLL_INTERVAL_SECS for single-cycle multiplication (2.0)
pub const INV_POLL_INTERVAL_SECS: f32 = 1.0 / POLL_INTERVAL_SECS;
/// Desktop GUI redraw and ring-buffer update interval in seconds
pub const RENDER_INTERVAL_SECS: f32 = 2.0;

/// Interval for checking INI file modifications
pub const INI_CHECK_INTERVAL_SECS: f32 = 10.0;
/// Count for checking INI file modifications
pub const INI_CHECK_COUNT: u32 = (INI_CHECK_INTERVAL_SECS / RENDER_INTERVAL_SECS) as u32;

pub const GRAPH_HISTORY_SAMPLES: usize = 120;
pub const DISK_INACTIVE_TIMEOUT_SECS: f32 = GRAPH_HISTORY_SAMPLES as f32 * RENDER_INTERVAL_SECS;
pub const TOP_PROCESS_COUNT: usize = 4;

/// Ratio of the disk row height used for the Read (up) graph
pub const DISK_GRAPH_READ_RATIO: f32 = 0.8;
/// Ratio of the disk row height used for the Write (down) graph
pub const DISK_GRAPH_WRITE_RATIO: f32 = 0.2;

/// Exponential Moving Average factor (0.1 = heavy smooth ~10 samples, 0.32 = balanced ~3-4 sub-samples, 1.0 = raw)
pub const CPU_SMOOTHING_ALPHA: f32 = 0.32;

/// Visibility Init Switches (Hide when idle / inactive)
/// When false, section still reserves its vertical space (no layout shift)
// Configuration is now loaded dynamically from win10_gadget.ini via config.rs

/// Whether to truncate long process names
pub const TRUNCATE_LONG_PROCESS_NAMES: bool = true;
/// Truncation style for long process names:
/// 1 = Character (crop exactly when character completes, no ellipsis)
/// 3 = EllipsisCharacter (crop and add "...")
/// 4 = EllipsisWord (crop at last fitting word and add "...")
pub const PROCESS_NAME_TRIMMING: i32 = 1;
/// The maximum width (in pixels) allowed for a process name before truncation (overrides layout calculations if smaller)
pub const MAX_PROCESS_NAME_WIDTH: f32 = 87.0;

/// Reciprocal multiplier to convert bytes to Megabytes (1 / (1024.0 * 1024.0))
pub const BYTES_TO_MB: f32 = 1.0 / 1_048_576.0;
/// Reciprocal multiplier to convert bytes to Gigabytes (1 / (1024.0 * 1024.0 * 1024.0))
pub const BYTES_TO_GB: f32 = 1.0 / 1_073_741_824.0;

// ============================================================================
//                                LUT OPTIMIZATIONS
// ============================================================================
static PCT_LUT: OnceLock<([u8; 101], [[u16; 4]; 101])> = OnceLock::new();

#[inline(always)]
fn get_pct_slice(pct: f32) -> &'static [u16] {
    let (lens, arr) = PCT_LUT.get_or_init(|| {
        let mut lens = [0u8; 101];
        let mut arr = [[0u16; 4]; 101];
        for i in 0..=100 {
            let s = format!("{}", i);
            let w: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
            lens[i as usize] = (w.len().saturating_sub(1)) as u8;
            for (j, &c) in w.iter().enumerate() {
                arr[i as usize][j] = c;
            }
        }
        (lens, arr)
    });
    let val = (pct + 0.5) as i32;
    let clamped = val.clamp(0, 100) as usize;
    &arr[clamped][..lens[clamped] as usize]
}

pub const WIDGET_WIDTH: i32 = 464;
pub const WIDGET_HEIGHT: i32 = 540;
pub const PADDING_TOP: i32 = 11;
pub const PADDING_RIGHT: i32 = 2;
pub const MARGIN_LEFT: i32 = 2;
/// Gap between sections, measured from the bottom of the previous section text

// Visually exact empty spacing between capital letters (based on an 8px grid system)
pub const GAP_LARGE: f32 = 32.0; // 4 * 8px
pub const GAP_SMALL: f32 = 24.0; // 3 * 8px

// Exact Segoe UI typography metrics (at 96 DPI)
// Title Font (24pt): Height 45, Ascent 36, Descent 9, InternalLeading 13
pub const TITLE_ASCENT: f32 = 36.0;
pub const TITLE_CAP_HEIGHT: f32 = 23.0; // Ascent(36) - InternalLeading(13)

// Body Font (8pt): Height 12, Ascent 10, Descent 2, InternalLeading 2
pub const BODY_ASCENT: f32 = 10.0;
pub const BODY_CAP_HEIGHT: f32 = 8.0;   // Ascent(10) - InternalLeading(2)
pub const BODY_INTERNAL_LEADING: f32 = 2.0;

// Pre-calculated offset for CPU/GPU temp
pub const TEMP_X_OFFSET: f32 = 0.0;
pub const TEMP_Y_OFFSET: f32 = 10.0;

/// Vertical Y-offset for disk Read and Write MB/s numbers (tiny_num)
pub const DISK_NUM_Y_OFFSET: f32 = 11.0;

pub const SECTION_GAP: i32 = 12;
/// Gap from a heading-only section title (Network, Disk) to its content below
pub const CONTENT_GAP: i32 = 20;

pub const FONT_NAME: &str = "Segoe UI";   // font family used for all text
/// Section label font size (pt) — used for RAM / CPU / GPU / Network / Disk headers
pub const FONT_SIZE_TITLE: f32 = 24.0;
/// Large per-section value font size (pt) — RAM GB, CPU %, GPU %
pub const FONT_SIZE_VALUE: f32 = 18.0;
/// Small body font size (pt) — process names, net/disk numbers, axis labels (bold)
pub const FONT_SIZE_BODY: f32 = 8.0;

/// Width of the title label column (section name: RAM, CPU, GPU)
pub const LABEL_COL_W: i32 = 110;
/// Width reserved for the large number next to the label (must fit e.g. "37.0" at FONT_SIZE_VALUE)
pub const VALUE_COL_W: i32 = 88;
/// Width of the process % / disk MB/s number column (right-aligned, ends at bar_start_x)
pub const PROC_NUM_W: i32 = 32;
/// Indent for process name rows and disk letter rows from MARGIN_LEFT
pub const PROCESS_INDENT: i32 = 46;
/// Width of RAM / CPU / GPU bars
pub const SECTION_BAR_W: i32 = 240;
/// Width of process mini-bars (shorter than section bars)
pub const MINI_BAR_W: i32 = 80;
/// Width of Network and Disk graphs; set equal to SECTION_BAR_W to align right edges
pub const BAR_W: i32 = 240;              // must be >= GRAPH_HISTORY_SAMPLES * 2
pub const BAR_H: i32 = 6;
/// Disk activity graph height per drive row
pub const DISK_GRAPH_H: i32 = 16;
/// Width of the axis label column to the LEFT of the net graph (scale labels)
pub const AXIS_LABEL_W: i32 = 28;
/// Width (px) reserved for decimal part suffix; integer parts right-align at bar_start_x() - AXIS_DEC_W
pub const AXIS_DEC_W: i32 = 18;
/// Maximum number of disk rows to reserve so hidden drives don't shift layout
pub const MAX_DISK_COUNT: usize = 6;

// Visual adjustment values for meters to center text vertically.
pub const LARGE_BAR_Y_OFFSET: f32 = -3.0;
pub const LARGE_NUM_X_OFFSET: f32 = -20.0;
pub const LARGE_NUM_Y_OFFSET: f32 = 3.0;
pub const SMALL_NUM_X_OFFSET: i32 = -6;

/// Width (px) allocated for the integer part of large numbers (RAM/CPU/GPU).
/// The decimal point sits at: MARGIN_LEFT + LABEL_COL_W + VALUE_INT_W.
/// VALUE_INT_W + VALUE_DEC_W must equal VALUE_COL_W.
pub const VALUE_INT_W: i32 = 56;
/// Width (px) for the decimal part of large numbers.
pub const VALUE_DEC_W: i32 = VALUE_COL_W - VALUE_INT_W;

/// Process bar scale: the bar fills 100% when CPU usage equals this percent.
/// Set to 10.0 so a 10% CPU process shows as a full bar.
pub const PROCESS_BAR_SCALE: f32 = 10.0;

pub const NET_SCALE_STEPS: &[f32] = &[
    0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10000.0, 20000.0, 50000.0, 100000.0,
];
pub const NET_SCALE_MIN: f32 = NET_SCALE_STEPS[0];
pub const DISK_SCALE_STEPS: &[f32] = NET_SCALE_STEPS.split_at(9).1;

pub const COLOR_TEXT: u32    = 0xFF_FFFFFF;  // fully opaque white — text, active bars, DL graph line
pub const COLOR_METER_UP: u32 = 0x59_FFFFFF;  // ~35% alpha white — background bars, UL graph line
pub const COLOR_TRANSPARENT: u32 = 0x00_00_00_00;

/// Precalculated DPI-scaled geometric metrics and font sizes.
#[derive(Debug, Clone, Copy)]
struct DpiLayout {
    scale: f32,
    widget_width: i32,
    widget_height: i32,
    padding_top: i32,
    padding_right: i32,
    margin_left: i32,

    adv_ram_cpu: i32,
    adv_cpu_proc: i32,
    adv_proc_vram: i32,
    adv_vram_gpu: i32,
    adv_gpu_net: i32,
    adv_net_graph: i32,
    adv_graph_disk: i32,
    adv_disk_c: i32,

    label_col_w: i32,
    value_col_w: i32,
    proc_num_w: i32,
    process_indent: i32,
    section_bar_w: i32,
    mini_bar_w: i32,
    bar_w: i32,
    bar_h: i32,
    axis_dec_w: i32,
    value_int_w: i32,
    font_size_title: f32,
    font_size_value: f32,
    font_size_body: f32,
    large_bar_y_offset: f32,
    large_num_x_offset: f32,
    large_num_y_offset: f32,
    small_num_x_offset: i32,
    #[allow(dead_code)] pub scale_1: f32,
    pub scale_2: f32,
    #[allow(dead_code)] pub scale_3: f32,
    #[allow(dead_code)] pub scale_4: f32,
    #[allow(dead_code)] pub scale_5: f32,
    pub scale_6: f32,
}

impl DpiLayout {
    fn new(dpi: u32) -> Self {
        let dpi = dpi.max(96);
        let scale = dpi as f32 / 96.0;

        // Pre-optimized pixel metrics (at 96 DPI scale 1.0)
        let body_h = (10.666 * scale).ceil() as i32;
        let scale_2_i32 = (2.0 * scale + 0.5) as i32;
        let proc_row_h = body_h + scale_2_i32;

        Self {
            scale,
            widget_width: (WIDGET_WIDTH as f32 * scale + 0.5) as i32,
            widget_height: (WIDGET_HEIGHT as f32 * scale + 0.5) as i32,
            padding_top: (PADDING_TOP as f32 * scale + 0.5) as i32,
            padding_right: (PADDING_RIGHT as f32 * scale + 0.5) as i32,
            margin_left: (MARGIN_LEFT as f32 * scale + 0.5) as i32,

            adv_ram_cpu: ((GAP_SMALL + TITLE_CAP_HEIGHT) * scale + 0.5) as i32,
            adv_cpu_proc: ((GAP_LARGE + GAP_SMALL) * scale + 0.5) as i32,
            adv_proc_vram: ((GAP_LARGE + TITLE_ASCENT) * scale + 0.5) as i32 - 4 * proc_row_h,
            adv_vram_gpu: ((GAP_SMALL + TITLE_CAP_HEIGHT) * scale + 0.5) as i32,
            adv_gpu_net: ((GAP_SMALL + TITLE_CAP_HEIGHT) * scale + 0.5) as i32,
            adv_net_graph: ((2.0 * GAP_SMALL + BODY_INTERNAL_LEADING) * scale + 0.5) as i32,
            adv_graph_disk: (BODY_CAP_HEIGHT * scale + 0.5) as i32,
            adv_disk_c: ((GAP_LARGE + GAP_SMALL) * scale + 0.5) as i32,

            label_col_w: (LABEL_COL_W as f32 * scale + 0.5) as i32,
            value_col_w: (VALUE_COL_W as f32 * scale + 0.5) as i32,
            proc_num_w: (PROC_NUM_W as f32 * scale + 0.5) as i32,
            process_indent: (PROCESS_INDENT as f32 * scale + 0.5) as i32,
            section_bar_w: (SECTION_BAR_W as f32 * scale + 0.5) as i32,
            mini_bar_w: (MINI_BAR_W as f32 * scale + 0.5) as i32,
            bar_w: (BAR_W as f32 * scale + 0.5) as i32,
            bar_h: (BAR_H as f32 * scale + 0.5) as i32,
            axis_dec_w: (AXIS_DEC_W as f32 * scale + 0.5) as i32,
            value_int_w: (VALUE_INT_W as f32 * scale + 0.5) as i32,
            font_size_title: FONT_SIZE_TITLE * scale,
            font_size_value: FONT_SIZE_VALUE * scale,
            font_size_body: FONT_SIZE_BODY * scale,
            large_bar_y_offset: LARGE_BAR_Y_OFFSET * scale,
            large_num_x_offset: LARGE_NUM_X_OFFSET * scale,
            large_num_y_offset: LARGE_NUM_Y_OFFSET * scale,
            small_num_x_offset: (SMALL_NUM_X_OFFSET as f32 * scale + 0.5) as i32,
            scale_1: ((1.0 * scale) + 0.5) as i32 as f32,
            scale_2: ((2.0 * scale) + 0.5) as i32 as f32,
            scale_3: ((3.0 * scale) + 0.5) as i32 as f32,
            scale_4: ((4.0 * scale) + 0.5) as i32 as f32,
            scale_5: ((5.0 * scale) + 0.5) as i32 as f32,
            scale_6: ((6.0 * scale) + 0.5) as i32 as f32,
        }
    }

    #[inline(always)]
    fn bar_start_x(&self) -> i32 {
        self.margin_left + self.label_col_w + self.value_col_w
    }

    #[inline(always)]
    fn little_num_int_x(&self) -> f32 {
        (self.bar_start_x() - self.axis_dec_w + self.small_num_x_offset) as f32
    }

    #[inline(always)]
    fn value_int_x(&self) -> f32 {
        (self.margin_left + self.label_col_w + self.value_int_w) as f32 + self.large_num_x_offset
    }
}

// ============================================================================
//                              RING BUFFER
// ============================================================================

#[derive(Clone, Copy)]
struct History120 {
    buf: [f32; GRAPH_HISTORY_SAMPLES],
    head: usize,
}

impl Default for History120 {
    fn default() -> Self {
        Self {
            buf: [0.0; GRAPH_HISTORY_SAMPLES],
            head: 0,
        }
    }
}

impl History120 {
    #[inline(always)]
    fn push(&mut self, v: f32) {
        self.buf[self.head] = v;
        self.head = if self.head + 1 >= GRAPH_HISTORY_SAMPLES { 0 } else { self.head + 1 };
    }

    #[inline(always)]
    fn get(&self, i: usize) -> f32 {
        debug_assert!(i < GRAPH_HISTORY_SAMPLES);
        let idx = self.head + i;
        let idx = if idx >= GRAPH_HISTORY_SAMPLES { idx - GRAPH_HISTORY_SAMPLES } else { idx };
        unsafe { *self.buf.get_unchecked(idx) }
    }

    #[inline(always)]
    fn max(&self) -> f32 {
        self.buf.iter().copied().fold(0.0_f32, f32::max)
    }

    #[inline(always)]
    fn zip_max(&self, other: &History120) -> f32 {
        self.buf.iter().zip(other.buf.iter())
            .map(|(&a, &b)| a.max(b))
            .fold(0.0_f32, f32::max)
    }

}

// ============================================================================
//                         METRICS DATA STRUCTURES
// ============================================================================

#[derive(Clone)]
struct ProcessMetric {
    name_wide: Vec<u16>,
    cpu_pct: f32,
}

struct ProcessState {
    name_wide: Vec<u16>,
    raw_cpu: f32,
    ema_cpu: f32,
    is_alive: bool,
}

#[derive(Clone, Copy)]
struct DiskMetricSnap {
    letter: char,
    read_mbps_wide: [u16; 8],
    write_mbps_wide: [u16; 8],
    read_history: History120,
    write_history: History120,
}

struct DiskMetric {
    letter: char,
    current_read_mbps: f32,
    current_write_mbps: f32,
    last_active: Option<Instant>,
    read_history: History120,
    write_history: History120,
}

struct MetricsSnapshot {
    ram_used_wide: Vec<u16>,
    ram_pct: f32,
    cpu_pct: f32,
    gpu_pct: f32,
    gpu_vram_used_wide: Vec<u16>,
    gpu_vram_pct: f32,
    cpu_temp_wide: Vec<u16>,
    gpu_temp_wide: Vec<u16>,
    net_down_history: History120,
    net_up_history: History120,
    net_scale_max_wide: Vec<u16>,
    net_scale_min_wide: Vec<u16>,
    top_processes: Vec<ProcessMetric>,
    disks: Vec<DiskMetricSnap>,
    total_disk_count: usize,
}

/// GDI+ and double-buffering resources scoped to the current monitor DPI.
struct RenderResources {
    layout: DpiLayout,
    font_family: *mut GpFontFamily,
    font_title: *mut GpFont,
    font_value: *mut GpFont,
    font_body: *mut GpFont,
    brush_text: *mut GpSolidFill,
    brush_meter: *mut GpSolidFill,
    brush_meter_up: *mut GpSolidFill,
    pen_meter: *mut GpPen,
    pen_meter_up: *mut GpPen,
    /// Cached pixel height of font_title (used for section spacing)
    title_h: f32,
    /// Cached pixel height of font_body (used for small-row spacing)
    body_h: f32,
    th: i32,
    proc_row_h: i32,
    proc_bar_y_offset: f32,
    meter_bar_y_offset: f32,
    net_total_h: i32,
    gpu_bg_visible: bool,
    proc_bg_visible: [bool; TOP_PROCESS_COUNT],
    /// Offscreen DC holding the pre-rendered static background layer (titles + 100% background bars)
    hdc_bg: HDC,
    hbitmap_bg: HBITMAP,
    graphics_bg: *mut GpGraphics,
    /// Fast 0-nanosecond pre-measured width lookup table for single-digit strings '0'..='9'
    digit_body_w: [f32; 10],
    digit_value_w: [f32; 10],
    format_ellipsis: *mut GpStringFormat,
    app_cfg: crate::config::AppConfig,
    temp_x_offset: f32,
    temp_y_offset: f32,
    disk_num_y_offset: f32,
}

impl RenderResources {
    unsafe fn destroy(&mut self) {
        GdipDeleteFontFamily(self.font_family);
        GdipDeleteFont(self.font_title);
        GdipDeleteFont(self.font_value);
        GdipDeleteFont(self.font_body);
        GdipDeleteBrush(self.brush_text as *mut _);
        GdipDeleteBrush(self.brush_meter as *mut _);
        GdipDeleteBrush(self.brush_meter_up as *mut _);
        GdipDeletePen(self.pen_meter);
        GdipDeletePen(self.pen_meter_up);
        GdipDeleteStringFormat(self.format_ellipsis);
        GdipDeleteGraphics(self.graphics_bg);
        let _ = DeleteDC(self.hdc_bg);
        let _ = DeleteObject(HGDIOBJ::from(self.hbitmap_bg));
    }

    unsafe fn new(layout: DpiLayout) -> Self {
        let mut font_family: *mut GpFontFamily = null_mut();
        GdipCreateFontFamilyFromName(
            w!("Segoe UI"),
            null_mut(),
            &mut font_family,
        );

        // Title: regular weight, large
        let mut font_title: *mut GpFont = null_mut();
        GdipCreateFont(font_family, layout.font_size_title, 0, UnitPoint, &mut font_title);

        // Value: regular weight, medium — for RAM/CPU/GPU numbers
        let mut font_value: *mut GpFont = null_mut();
        GdipCreateFont(font_family, layout.font_size_value, 0, UnitPoint, &mut font_value);

        // Body: Bold weight (FontStyleBold = 1) — for process names, small numbers
        let mut font_body: *mut GpFont = null_mut();
        GdipCreateFont(font_family, layout.font_size_body, 1, UnitPoint, &mut font_body);

        let mut brush_text: *mut GpSolidFill = null_mut();
        let mut brush_meter: *mut GpSolidFill = null_mut();
        let mut brush_meter_up: *mut GpSolidFill = null_mut();
        GdipCreateSolidFill(COLOR_TEXT, &mut brush_text);
        GdipCreateSolidFill(COLOR_TEXT, &mut brush_meter);
        GdipCreateSolidFill(COLOR_METER_UP, &mut brush_meter_up);

        let mut pen_meter: *mut GpPen = null_mut();
        let mut pen_meter_up: *mut GpPen = null_mut();
        GdipCreatePen1(COLOR_TEXT, 1.0, UnitPixel, &mut pen_meter);
        GdipCreatePen1(COLOR_METER_UP, 1.0, UnitPixel, &mut pen_meter_up);

        // Pre-measure single-digit widths ('0'..='9') for body and value fonts
        let mut title_h = 0.0f32;
        let mut body_h = 0.0f32;
        let mut digit_body_w = [0.0f32; 10];
        let mut digit_value_w = [0.0f32; 10];
        {
            let hdc = GetDC(Option::<HWND>::None);
            let mut tmp_g: *mut GpGraphics = null_mut();
            GdipCreateFromHDC(hdc, &mut tmp_g);
            GdipGetFontHeight(font_title, tmp_g, &mut title_h);
            GdipGetFontHeight(font_body, tmp_g, &mut body_h);

            for d in 0..10 {
                let w_str = [(b'0' + d as u8) as u16, 0];
                digit_body_w[d] = measure_str_raw_wide(tmp_g, &w_str, font_body, body_h * 2.0);
                digit_value_w[d] = measure_str_raw_wide(tmp_g, &w_str, font_value, title_h * 1.5);
            }

            GdipDeleteGraphics(tmp_g);
            ReleaseDC(Option::<HWND>::None, hdc);
        }

        // Create pre-rendered static background layer DC & Bitmap
        let hdc_screen = GetDC(Option::<HWND>::None);
        let hdc_bg = CreateCompatibleDC(Some(hdc_screen));
        let hbitmap_bg = CreateCompatibleBitmap(hdc_screen, layout.widget_width, layout.widget_height + 400);
        let _ = SelectObject(hdc_bg, HGDIOBJ::from(hbitmap_bg));
        ReleaseDC(Option::<HWND>::None, hdc_screen);

        let mut graphics_bg: *mut GpGraphics = null_mut();
        GdipCreateFromHDC(hdc_bg, &mut graphics_bg);
        GdipSetSmoothingMode(graphics_bg, SmoothingModeAntiAlias);
        GdipSetTextRenderingHint(graphics_bg, TextRenderingHintAntiAlias);
        GdipGraphicsClear(graphics_bg, COLOR_TRANSPARENT);

        let mut format_ellipsis: *mut GpStringFormat = null_mut();
        // 4096 is StringFormatFlagsNoWrap, ensuring text is strictly confined to 1 line
        GdipCreateStringFormat(4096, 0, &mut format_ellipsis);
        GdipSetStringFormatTrimming(format_ellipsis, std::mem::transmute(PROCESS_NAME_TRIMMING));
        GdipSetStringFormatAlign(format_ellipsis, StringAlignmentNear);

        let th = title_h.ceil() as i32;
        let bh = body_h.ceil() as i32;
        let proc_row_h = bh + layout.scale_2 as i32;
        let proc_bar_y_offset = body_h * (10.0 / 12.0) - layout.bar_h as f32 - layout.scale_2;
        let meter_bar_y_offset = title_h * 0.5 - layout.bar_h as f32 * 0.5;
        let net_total_h = TOP_PROCESS_COUNT as i32 * proc_row_h;

        let app_cfg = crate::config::CONFIG.read().unwrap().clone();
        let gpu_bg_visible = !app_cfg.gpu_hide_when_idle || app_cfg.gpu_always_visible;
        let mut proc_bg_visible = [true; TOP_PROCESS_COUNT];
        for i in 0..TOP_PROCESS_COUNT { proc_bg_visible[i] = !app_cfg.process_hide_bars_when_idle; }
        let temp_x_offset = TEMP_X_OFFSET * layout.scale;
        let temp_y_offset = TEMP_Y_OFFSET * layout.scale;
        let disk_num_y_offset = DISK_NUM_Y_OFFSET * layout.scale;

        let mut res_static = Self {
            layout,
            font_family,
            font_title,
            font_value,
            font_body,
            brush_text,
            brush_meter,
            brush_meter_up,
            pen_meter,
            pen_meter_up,
            title_h,
            body_h,
            th,
            proc_row_h,
            proc_bar_y_offset,
            meter_bar_y_offset,
            net_total_h,
            gpu_bg_visible,
            proc_bg_visible,
            hdc_bg,
            hbitmap_bg,
            graphics_bg,
            digit_body_w,
            digit_value_w,
            format_ellipsis,
            app_cfg: app_cfg.clone(),
            temp_x_offset,
            temp_y_offset,
            disk_num_y_offset,
        };

        let bar_x = layout.bar_start_x() as f32;
        let bar_mid = title_h * 0.5;
        let section_bar_w = layout.section_bar_w as f32;
        let bar_h = layout.bar_h as f32;

        let mut y = layout.padding_top;

        let ram_title = [b'R' as u16, b'A' as u16, b'M' as u16, 0];
        draw_title(graphics_bg, &mut res_static, &ram_title, layout.margin_left as f32, y as f32, layout.label_col_w as f32);
        let bar_y = ((y as f32 + bar_mid - bar_h * 0.5 + layout.large_bar_y_offset) + 0.5) as i32 as f32;
        GdipFillRectangle(graphics_bg, brush_meter_up as _, bar_x, bar_y, section_bar_w, bar_h);
        y += layout.adv_ram_cpu;

        let cpu_title = [b'C' as u16, b'P' as u16, b'U' as u16, 0];
        draw_title(graphics_bg, &mut res_static, &cpu_title, layout.margin_left as f32, y as f32, layout.label_col_w as f32);
        let bar_y = ((y as f32 + bar_mid - bar_h * 0.5 + layout.large_bar_y_offset) + 0.5) as i32 as f32;
        GdipFillRectangle(graphics_bg, brush_meter_up as _, bar_x, bar_y, section_bar_w, bar_h);
        y += layout.adv_cpu_proc;

        for i in 0..TOP_PROCESS_COUNT {
            if proc_bg_visible[i] {
                let bar_y = ((y as f32 + res_static.proc_bar_y_offset) + 0.5) as i32 as f32;
                GdipFillRectangle(graphics_bg, brush_meter_up as _, bar_x, bar_y, layout.mini_bar_w as f32, bar_h);
            }
            y += proc_row_h;
        }
        y += layout.adv_proc_vram;

        // VRAM
        let vram_title = [b'V' as u16, b'R' as u16, b'A' as u16, b'M' as u16, 0];
        draw_title(graphics_bg, &mut res_static, &vram_title, layout.margin_left as f32, y as f32, layout.label_col_w as f32);
        let bar_y_vram = ((y as f32 + bar_mid - bar_h * 0.5 + layout.large_bar_y_offset) + 0.5) as i32 as f32;
        GdipFillRectangle(graphics_bg, brush_meter_up as _, bar_x, bar_y_vram, section_bar_w, bar_h);
        y += layout.adv_vram_gpu;

        if !app_cfg.gpu_hide_when_idle || app_cfg.gpu_always_visible {
            let gpu_title = [b'G' as u16, b'P' as u16, b'U' as u16, 0];
            draw_title(graphics_bg, &mut res_static, &gpu_title, layout.margin_left as f32, y as f32, layout.label_col_w as f32);
            let bar_y = ((y as f32 + bar_mid - bar_h * 0.5 + layout.large_bar_y_offset) + 0.5) as i32 as f32;
            GdipFillRectangle(graphics_bg, brush_meter_up as _, bar_x, bar_y, section_bar_w, bar_h);
        }
        y += layout.adv_gpu_net;

        let net_title = [b'N' as u16, b'e' as u16, b't' as u16, b'w' as u16, b'o' as u16, b'r' as u16, b'k' as u16, 0];
        draw_title(graphics_bg, &mut res_static, &net_title, layout.margin_left as f32, y as f32, (layout.widget_width - layout.margin_left - layout.padding_right) as f32);
        let _ = y;
        
        res_static
    }
}



// ============================================================================
//                             UTILITY FUNCTIONS
// ============================================================================

#[inline(always)]
fn trim_working_set() {
    unsafe {
        let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}

#[inline(always)]
fn hash_bytes(bytes: &[u8]) -> u64 {
    use std::hash::Hasher;
    let mut hasher = rustc_hash::FxHasher::default();
    hasher.write(bytes);
    hasher.finish()
}

#[inline(always)]
fn adaptive_ema(old_val: f32, new_val: f32, default_alpha: f32) -> f32 {
    if old_val == 0.0 {
        return new_val;
    }
    let delta = (new_val - old_val).abs();
    let alpha = if delta > 20.0 { 0.60 } else { default_alpha };
    old_val * (1.0 - alpha) + new_val * alpha
}

/// Zero-allocation float formatter directly writing UTF-16 characters into a reusable buffer
fn format_float_to_wide(val: f32, max_decimals: usize, out: &mut Vec<u16>) {
    out.clear();
    let mut stack_buf = [0u8; 32];
    use std::io::Write;
    let mut cursor = std::io::Cursor::new(&mut stack_buf[..]);
    match max_decimals {
        0 => { let _ = write!(cursor, "{:.0}", val); }
        1 => { let _ = write!(cursor, "{:.1}", val); }
        2 => { let _ = write!(cursor, "{:.2}", val); }
        _ => { let _ = write!(cursor, "{}", val); }
    }
    let len = cursor.position() as usize;
    if let std::result::Result::Ok(s) = std::str::from_utf8(&stack_buf[..len]) {
        let trimmed = if s.contains('.') {
            let t = s.trim_end_matches('0').trim_end_matches('.');
            if t.is_empty() { "0" } else { t }
        } else {
            s
        };
        out.extend(trimmed.encode_utf16());
        out.push(0);
    }
}

/// Zero-allocation temperature formatter directly writing UTF-16 characters into a reusable buffer
fn format_temp_to_wide(temp: f32, suffix: &str, out: &mut Vec<u16>) {
    out.clear();
    let mut stack_buf = [0u8; 32];
    use std::io::Write;
    let mut cursor = std::io::Cursor::new(&mut stack_buf[..]);
    let _ = write!(cursor, "{:.0}{}", temp, suffix);
    let len = cursor.position() as usize;
    if let std::result::Result::Ok(s) = std::str::from_utf8(&stack_buf[..len]) {
        out.extend(s.encode_utf16());
        out.push(0);
    }
}

fn scale_f32(val: f32, max: f32, target: f32) -> f32 {
    (val / max.max(0.001)).clamp(0.0, 1.0) * target
}

fn net_scale_max(down: &History120, up: &History120) -> f32 {
    let peak = down.zip_max(up).max(NET_SCALE_MIN);
    NET_SCALE_STEPS
        .iter()
        .copied()
        .find(|&s| s >= peak)
        .unwrap_or(1000.0)
}

fn disk_scale_max(history: &History120) -> f32 {
    let peak = history.max();
    DISK_SCALE_STEPS
        .iter()
        .copied()
        .find(|&s| s >= peak)
        .unwrap_or(5000.0)
}




unsafe fn clear_bg_rect(graphics_bg: *mut GpGraphics, y: i32, h: i32, width: i32) {
    let mut brush_clear: *mut GpSolidFill = std::ptr::null_mut();
    GdipCreateSolidFill(COLOR_TRANSPARENT, &mut brush_clear);
    GdipSetCompositingMode(graphics_bg, CompositingModeSourceCopy);
    GdipFillRectangle(graphics_bg, brush_clear as _, 0.0, y as f32, width as f32, h as f32);
    GdipSetCompositingMode(graphics_bg, CompositingModeSourceOver);
    GdipDeleteBrush(brush_clear as _);
}

fn is_virtual_or_loopback(name: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "loopback", "vethernet", "virtual", "vmware", "vbox", "npcap", "tap-", "pseudo",
    ];
    let bytes = name.as_bytes();
    if bytes.eq_ignore_ascii_case(b"lo") {
        return true;
    }
    for &kw in KEYWORDS {
        let kw_b = kw.as_bytes();
        if bytes.len() >= kw_b.len()
            && bytes.windows(kw_b.len()).any(|w| w.eq_ignore_ascii_case(kw_b))
        {
            return true;
        }
    }
    false
}

/// Zero-allocation UTF-16 string conversion helper into a reusable buffer
fn to_wide_buf(s: &str, out: &mut Vec<u16>) {
    out.clear();
    out.extend(s.encode_utf16());
    out.push(0);
}

/// Zero-allocation integer to null-terminated UTF-16 conversion into a stack-allocated inline array
#[inline(always)]
fn int_to_wide_fixed(val: i32) -> [u16; 8] {
    let mut buf = [0u16; 8];
    let mut n = val.max(0);
    if n == 0 {
        buf[0] = b'0' as u16;
        buf[1] = 0;
        return buf;
    }
    let mut digits = [0u16; 8];
    let mut idx = 0;
    while n > 0 && idx < 7 {
        digits[idx] = (b'0' + (n % 10) as u8) as u16;
        n /= 10;
        idx += 1;
    }
    let mut out_idx = 0;
    while idx > 0 {
        idx -= 1;
        buf[out_idx] = digits[idx];
        out_idx += 1;
    }
    buf[out_idx] = 0;
    buf
}

fn to_wide(s: &str) -> Vec<u16> {
    let mut buf = Vec::with_capacity(s.len() + 1);
    to_wide_buf(s, &mut buf);
    buf
}

// ============================================================================
//                        MONITOR & WIN32 DESKTOP UTILS
// ============================================================================

#[derive(Clone, Copy, Debug)]
struct MonitorInfo {
    rect: RECT,
    dpi: u32,
}

unsafe extern "system" fn enum_monitors_proc(
    hmonitor: windows::Win32::Graphics::Gdi::HMONITOR,
    _: HDC,
    _: *mut RECT,
    lparam: LPARAM,
) -> windows::core::BOOL {
    let monitors = &mut *(lparam.0 as *mut Vec<MonitorInfo>);
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(hmonitor, &mut mi).as_bool() {
        let mut dpi_x = 96u32;
        let mut dpi_y = 96u32;
        let dpi = if GetDpiForMonitor(hmonitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y).is_ok() {
            dpi_x
        } else {
            GetDpiForSystem()
        };
        monitors.push(MonitorInfo { rect: mi.rcMonitor, dpi });
    }
    true.into()
}

fn get_active_monitors() -> Vec<MonitorInfo> {
    let mut monitors: Vec<MonitorInfo> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            Option::<HDC>::None,
            None,
            Some(enum_monitors_proc),
            LPARAM(&mut monitors as *mut _ as isize),
        );
    }
    if monitors.is_empty() {
        unsafe {
            let cx = GetSystemMetrics(SM_CXSCREEN);
            let cy = GetSystemMetrics(SM_CYSCREEN);
            let dpi = GetDpiForSystem();
            vec![MonitorInfo {
                rect: RECT { left: 0, top: 0, right: cx, bottom: cy },
                dpi,
            }]
        }
    } else {
        monitors
    }
}

unsafe fn find_workerw() -> HWND {
    let mut workerw = HWND::default();
    unsafe extern "system" fn enum_windows_proc(top_hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
        let p_shell = FindWindowExW(
            Some(top_hwnd),
            Some(HWND::default()),
            w!("SHELLDLL_DefView"),
            PCWSTR::null(),
        )
        .unwrap_or_default();

        if p_shell != HWND::default() {
            let worker = FindWindowExW(
                Some(HWND::default()),
                Some(top_hwnd),
                w!("WorkerW"),
                PCWSTR::null(),
            )
            .unwrap_or_default();
            if worker != HWND::default() {
                *(lparam.0 as *mut HWND) = worker;
            }
        }
        true.into()
    }
    let _ = EnumWindows(Some(enum_windows_proc), LPARAM(&mut workerw as *mut _ as isize));
    workerw
}

unsafe fn attach_to_desktop(hwnd: HWND) {
    if !ATTACH_TO_WORKERW {
        let z_order = if ALWAYS_ON_TOP { HWND_TOPMOST } else { HWND_BOTTOM };
        let _ = SetWindowPos(
            hwnd,
            Some(z_order),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
        #[cfg(debug_assertions)]
        eprintln!("[Gadget] Top-level desktop window initialized at HWND_BOTTOM (ALWAYS_ON_TOP = {})", ALWAYS_ON_TOP);
        return;
    }

    // Active wallpaper re-parenting mode
    let mut workerw = find_workerw();
    if workerw == HWND::default() {
        let progman = FindWindowW(w!("Progman"), PCWSTR::null()).unwrap_or_default();
        if progman != HWND::default() {
            let mut result = 0;
            let _ = SendMessageTimeoutW(
                progman,
                0x052C,
                WPARAM(0x0D),
                LPARAM(0),
                SMTO_NORMAL,
                1000,
                Some(&mut result),
            );
            workerw = find_workerw();
        }
    }
    if workerw != HWND::default() {
        let _ = SetParent(hwnd, Some(workerw));
        #[cfg(debug_assertions)]
        eprintln!("[Gadget] Attached as child of WorkerW ({:?})", workerw);
    } else {
        let z_order = if ALWAYS_ON_TOP { HWND_TOPMOST } else { HWND_BOTTOM };
        let _ = SetWindowPos(
            hwnd,
            Some(z_order),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
        #[cfg(debug_assertions)]
        eprintln!("[Gadget Warning] WorkerW not found. Displayed as top-level window at HWND_BOTTOM.");
    }
}

// ============================================================================
//                             SYSTEM MONITORING
// ============================================================================

fn start_metrics_collector(tx: mpsc::Sender<MetricsSnapshot>, hwnd_val: isize) {
    std::thread::spawn(move || {
        unsafe {
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
        }

        let mut sys = System::new();
        sys.refresh_memory();
        sys.refresh_cpu_all();

        let mut networks = Networks::new_with_refreshed_list();
        let mut disks = Disks::new_with_refreshed_list();

        let mut net_down_hist = History120::default();
        let mut net_up_hist = History120::default();
        let mut disk_metrics: FxHashMap<char, DiskMetric> = FxHashMap::default();

        let mut smoothed_cpu = 0.0f32;
        let mut smoothed_gpu = 0.0f32;
        let mut smoothed_net_down = 0.0f32;
        let mut smoothed_net_up = 0.0f32;
        let mut proc_map: FxHashMap<String, ProcessState> = FxHashMap::default();

        let mut total_vram_bytes = 0.0f32;
        unsafe {
            if let std::result::Result::Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() {
                let mut i = 0;
                while let std::result::Result::Ok(adapter) = factory.EnumAdapters1(i) {
                    if let std::result::Result::Ok(desc) = adapter.GetDesc1() {
                        if (desc.Flags & (DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32)) == 0 {
                            total_vram_bytes += desc.DedicatedVideoMemory as f32;
                        }
                    }
                    i += 1;
                }
            }
        }

        // Windows PDH GPU counter query setup
        let mut hquery = PDH_HQUERY::default();
        let mut hcounter_gpu = PDH_HCOUNTER::default();
        let mut hcounter_vram = PDH_HCOUNTER::default();
        let mut hcounter_cpu_utility = PDH_HCOUNTER::default();
        unsafe {
            let _ = PdhOpenQueryW(PCWSTR::null(), 0, &mut hquery);

            PdhAddEnglishCounterW(
                hquery,
                w!("\\GPU Engine(*)\\Utilization Percentage"),
                0,
                &mut hcounter_gpu,
            );

            PdhAddEnglishCounterW(
                hquery,
                w!("\\GPU Adapter Memory(*)\\Dedicated Usage"),
                0,
                &mut hcounter_vram,
            );

            PdhAddEnglishCounterW(
                hquery,
                w!("\\Processor Information(_Total)\\% Processor Utility"),
                0,
                &mut hcounter_cpu_utility,
            );
            PdhCollectQueryData(hquery);
        }

        let mut smi: Option<all_smi::prelude::AllSmi> = None;
        let mut last_cpu_temp = 0.0;
        let mut last_gpu_temp = 0.0;
        
        // --- Warmup Phase ---
        // Wait exactly 1 poll interval so the very first frame sent to the GUI contains real delta values
        // rather than 0s, preventing the 2-second visual delay before numbers pop in.
        sys.refresh_cpu_all();
        sys.refresh_processes_specifics(sysinfo::ProcessesToUpdate::All, true, sysinfo::ProcessRefreshKind::nothing().with_cpu());
        std::thread::sleep(std::time::Duration::from_secs_f32(POLL_INTERVAL_SECS));

        let mut last_tick = Instant::now();
        let mut app_cfg = crate::config::load_or_create_ini();
        let mut steps_per_render = (app_cfg.render_interval_secs * INV_POLL_INTERVAL_SECS + 0.5) as usize;
        let mut ini_check_poll_steps = (app_cfg.ini_check_interval_secs * INV_POLL_INTERVAL_SECS + 0.5) as usize;
        let mut disk_inactive_timeout_secs = GRAPH_HISTORY_SAMPLES as f32 * app_cfg.render_interval_secs;

        let mut step_count = 0usize;
        let mut first_frame = true;
        let mut last_ini_hash: u64 = if let std::result::Result::Ok(bytes) = std::fs::read(crate::config::get_ini_path()) {
            hash_bytes(&bytes)
        } else {
            0
        };
        let mut pdh_gpu_buf: Vec<u8> = Vec::with_capacity(512);
        let mut pdh_vram_buf: Vec<u8> = Vec::with_capacity(512);

        let mut ini_poll_counter = 0usize;

        // Preallocated reusable scratch buffers to ensure zero heap allocations per tick
        let mut proc_list: Vec<ProcessMetric> = Vec::with_capacity(TOP_PROCESS_COUNT);
        let mut disks_sorted: Vec<DiskMetricSnap> = Vec::with_capacity(MAX_DISK_COUNT);

        let mut net_scale_max_wide = Vec::with_capacity(8);
        let mut net_scale_min_wide = Vec::with_capacity(8);
        let mut ram_used_wide = Vec::with_capacity(8);
        let mut gpu_vram_used_wide = Vec::with_capacity(8);
        let mut cpu_temp_wide = Vec::with_capacity(8);
        let mut gpu_temp_wide = Vec::with_capacity(8);

        loop {
            // Sleep / Modern Standby power awareness: pause background work when OS enters sleep/suspend
            if IS_SYSTEM_SUSPENDED.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(500));
                // Recalibrate delta clocks and warm up CPU counters so waking frame generates zero spikes
                last_tick = Instant::now();
                sys.refresh_cpu_all();
                sys.refresh_processes_specifics(
                    sysinfo::ProcessesToUpdate::All,
                    true,
                    sysinfo::ProcessRefreshKind::nothing().with_cpu(),
                );
                continue;
            }

            let loop_start = Instant::now();

            // Check INI modification or manual reload request
            ini_poll_counter += 1;
            let reload_requested = CONFIG_RELOAD_REQUESTED.swap(false, Ordering::Relaxed);
            if reload_requested || ini_poll_counter >= ini_check_poll_steps {
                ini_poll_counter = 0;
                let target_ini_path = crate::config::get_ini_path();
                if let std::result::Result::Ok(bytes) = std::fs::read(&target_ini_path) {
                    let current_hash = hash_bytes(&bytes);
                    if reload_requested || current_hash != last_ini_hash {
                        let new_cfg = crate::config::load_or_create_ini();
                        app_cfg = new_cfg;
                        steps_per_render = (app_cfg.render_interval_secs * INV_POLL_INTERVAL_SECS + 0.5) as usize;
                        ini_check_poll_steps = (app_cfg.ini_check_interval_secs * INV_POLL_INTERVAL_SECS + 0.5) as usize;
                        disk_inactive_timeout_secs = GRAPH_HISTORY_SAMPLES as f32 * app_cfg.render_interval_secs;
                        step_count = steps_per_render; // Force immediate GUI frame update on next tick
                        update_startup_registry(app_cfg.run_on_startup);

                        if hwnd_val != 0 && !reload_requested {
                            unsafe {
                                let _ = PostMessageW(Some(HWND(hwnd_val as _)), WM_USER_CONFIG_RELOADED, WPARAM(0), LPARAM(0));
                            }
                        }

                        last_ini_hash = current_hash;
                    }
                }
                trim_working_set();
            }

            let is_render_step = first_frame || {
                step_count += 1;
                step_count >= steps_per_render
            };
            if is_render_step {
                step_count = 0;
                first_frame = false;
            }

            let elapsed = last_tick.elapsed().as_secs_f32().max(0.05);
            let inv_elapsed = 1.0 / elapsed;
            last_tick = Instant::now();

            sys.refresh_memory();
            sys.refresh_cpu_all();
            sys.refresh_processes_specifics(ProcessesToUpdate::All, true, sysinfo::ProcessRefreshKind::nothing().with_cpu());
            networks.refresh(true);
            disks.refresh(true);

            // Standard hardware core count normalization across system threads
            let total_cpus = sys.cpus().len().max(1) as f32;

            let used_ram = sys.used_memory() as f32 * BYTES_TO_GB;
            let total_ram = sys.total_memory() as f32 * BYTES_TO_GB;
            let _ram_pct = (used_ram / total_ram.max(0.1)) * 100.0;

            let pdh_cpu_utility = unsafe {
                let mut type_ = 0;
                let mut val: PDH_FMT_COUNTERVALUE = std::mem::zeroed();
                if PdhGetFormattedCounterValue(hcounter_cpu_utility, PDH_FMT_DOUBLE, Some(&mut type_), &mut val) == ERROR_SUCCESS.0 {
                    val.Anonymous.doubleValue as f32
                } else {
                    0.0
                }
            };

            // Raw global CPU + Adaptive EMA smoothing filter (snaps fast on spikes, smooth on idle)
            let raw_cpu = if app_cfg.cpu_taskmgr_percentage_logic && pdh_cpu_utility > 0.0 {
                pdh_cpu_utility.clamp(0.0, 100.0)
            } else {
                sys.global_cpu_usage().clamp(0.0, 100.0)
            };
            smoothed_cpu = adaptive_ema(smoothed_cpu, raw_cpu, CPU_SMOOTHING_ALPHA);

            let mut total_rx = 0u64;
            let mut total_tx = 0u64;
            for (name, net) in &networks {
                if is_virtual_or_loopback(name) {
                    continue;
                }
                total_rx += net.received();
                total_tx += net.transmitted();
            }
            let net_down_mbps = (total_rx as f32 * BYTES_TO_MB) * inv_elapsed;
            let net_up_mbps = (total_tx as f32 * BYTES_TO_MB) * inv_elapsed;
            smoothed_net_down = adaptive_ema(smoothed_net_down, net_down_mbps, CPU_SMOOTHING_ALPHA);
            smoothed_net_up = adaptive_ema(smoothed_net_up, net_up_mbps, CPU_SMOOTHING_ALPHA);

            // Read PDH GPU wildcard array (sum active GPU 3D/Compute engines)
            let mut raw_gpu = 0.0f32;
            unsafe {
                if PdhCollectQueryData(hquery) == ERROR_SUCCESS.0 {
                    let mut sum = 0.0;
                    let mut buf_size = 0;
                    let mut count = 0;
                    PdhGetFormattedCounterArrayW(hcounter_gpu, PDH_FMT_DOUBLE, &mut buf_size, &mut count, None);
                    if buf_size > 0 {
                        let req = buf_size as usize;
                        if pdh_gpu_buf.len() < req {
                            pdh_gpu_buf.resize(req, 0);
                        }
                        if PdhGetFormattedCounterArrayW(
                            hcounter_gpu,
                            PDH_FMT_DOUBLE,
                            &mut buf_size,
                            &mut count,
                            Some(pdh_gpu_buf.as_mut_ptr() as *mut _),
                        ) == ERROR_SUCCESS.0
                        {
                            let items = std::slice::from_raw_parts(pdh_gpu_buf.as_ptr() as *const PDH_FMT_COUNTERVALUE_ITEM_W, count as usize);
                            for item in items {
                                sum += item.FmtValue.Anonymous.doubleValue;
                            }
                        }
                    }
                    raw_gpu = sum as f32;
                }
            }
            raw_gpu = raw_gpu.clamp(0.0, 100.0);
            smoothed_gpu = adaptive_ema(smoothed_gpu, raw_gpu, CPU_SMOOTHING_ALPHA);

            let raw_vram_bytes = unsafe {
                let mut sum = 0.0;
                let mut buf_size = 0;
                let mut count = 0;
                PdhGetFormattedCounterArrayW(hcounter_vram, PDH_FMT_DOUBLE, &mut buf_size, &mut count, None);
                if buf_size > 0 {
                    let req = buf_size as usize;
                    if pdh_vram_buf.len() < req {
                        pdh_vram_buf.resize(req, 0);
                    }
                    if PdhGetFormattedCounterArrayW(
                        hcounter_vram,
                        PDH_FMT_DOUBLE,
                        &mut buf_size,
                        &mut count,
                        Some(pdh_vram_buf.as_mut_ptr() as *mut _),
                    ) == ERROR_SUCCESS.0
                    {
                        let items = std::slice::from_raw_parts(pdh_vram_buf.as_ptr() as *const PDH_FMT_COUNTERVALUE_ITEM_W, count as usize);
                        for item in items {
                            sum += item.FmtValue.Anonymous.doubleValue;
                        }
                    }
                }
                sum as f32
            };
            
            let mut cpu_temp = last_cpu_temp;
            let mut gpu_temp = last_gpu_temp;

            if app_cfg.show_temperatures && is_render_step {
                if smi.is_none() {
                    smi = all_smi::prelude::AllSmi::new().ok();
                }
                if let Some(s) = &mut smi {
                    if app_cfg.show_cpu_temp {
                        let mut max_cpu = 0u32;
                        for cpu in s.get_cpu_info() {
                            if let Some(t) = cpu.temperature {
                                if t > max_cpu { max_cpu = t; }
                            }
                        }
                        if max_cpu > 0 {
                            cpu_temp = max_cpu as f32;
                            last_cpu_temp = max_cpu as f32;
                        }
                    }
                    
                    if app_cfg.show_gpu_temp {
                        let mut max_gpu = 0u32;
                        for gpu in s.get_gpu_info() {
                            let t = gpu.temperature as u32;
                            if t > max_gpu { max_gpu = t; }
                        }
                        if max_gpu > 0 {
                            gpu_temp = max_gpu as f32;
                            last_gpu_temp = max_gpu as f32;
                        }
                    }
                }
            }

            // Process CPU usage normalized by CPU thread count (summing up cleanly to total CPU %)
            // We first aggregate same-named processes so they sum their CPU usage, which fixes EMA key collisions
            for state in proc_map.values_mut() {
                state.raw_cpu = 0.0;
                state.is_alive = false;
            }
            for p in sys.processes().values() {
                let raw_name = p.name().to_str().unwrap_or("").trim_end_matches(".exe");
                let raw_proc_cpu = (p.cpu_usage() / total_cpus).clamp(0.0, 100.0);
                if let Some(state) = proc_map.get_mut(raw_name) {
                    state.raw_cpu += raw_proc_cpu;
                    state.is_alive = true;
                } else if raw_proc_cpu > 0.0 || !app_cfg.process_hide_when_idle {
                    proc_map.insert(
                        raw_name.to_string(),
                        ProcessState {
                            name_wide: to_wide(raw_name),
                            raw_cpu: raw_proc_cpu,
                            ema_cpu: raw_proc_cpu,
                            is_alive: true,
                        },
                    );
                }
            }
            
            proc_map.retain(|_, state| {
                // If the process is dead (not found in current running processes), purge it immediately!
                if !state.is_alive {
                    return false;
                }
                state.ema_cpu = adaptive_ema(state.ema_cpu, state.raw_cpu, CPU_SMOOTHING_ALPHA);
                !app_cfg.process_hide_when_idle || state.ema_cpu >= 0.1
            });

            let mut proc_borrow: Vec<(&Vec<u16>, f32)> = Vec::with_capacity(64);
            for state in proc_map.values() {
                proc_borrow.push((&state.name_wide, state.ema_cpu));
            }

            proc_borrow.sort_unstable_by(|a, b| {
                b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
            });

            if app_cfg.process_hide_when_idle {
                proc_borrow.retain(|p| (p.1 + 0.5) as i32 > 0);
            }
            proc_borrow.truncate(TOP_PROCESS_COUNT);

            let now = Instant::now();

            // Direct per-disk I/O monitoring with zero-allocation letter parsing
            for disk in &disks {
                let letter_char = disk
                    .mount_point()
                    .to_str()
                    .and_then(|s| s.chars().next())
                    .unwrap_or('?')
                    .to_ascii_uppercase();
                let is_c = letter_char == 'C';

                let du = disk.usage();
                let raw_disk_read_mbps = (du.read_bytes as f32 * BYTES_TO_MB) * inv_elapsed;
                let raw_disk_write_mbps = (du.written_bytes as f32 * BYTES_TO_MB) * inv_elapsed;

                let entry = disk_metrics
                    .entry(letter_char)
                    .or_insert_with(|| DiskMetric {
                        letter: letter_char,
                        current_read_mbps: 0.0,
                        current_write_mbps: 0.0,
                        last_active: if is_c { Some(now) } else { None },
                        read_history: History120::default(),
                        write_history: History120::default(),
                    });

                entry.current_read_mbps = adaptive_ema(entry.current_read_mbps, raw_disk_read_mbps, CPU_SMOOTHING_ALPHA);
                entry.current_write_mbps = adaptive_ema(entry.current_write_mbps, raw_disk_write_mbps, CPU_SMOOTHING_ALPHA);
                if entry.current_read_mbps > 0.05 || entry.current_write_mbps > 0.05 || is_c {
                    entry.last_active = Some(now);
                }
            }

            // Trigger snapshot push & history push only on render interval boundary (2.0s)
            if is_render_step {
                proc_list.clear();
                for (name_wide, cpu) in &proc_borrow {
                    proc_list.push(ProcessMetric {
                        name_wide: (*name_wide).clone(),
                        cpu_pct: *cpu,
                    });
                }

                net_down_hist.push(smoothed_net_down);
                net_up_hist.push(smoothed_net_up);

                for d in disk_metrics.values_mut() {
                    d.read_history.push(d.current_read_mbps);
                    d.write_history.push(d.current_write_mbps);
                }

                disks_sorted.clear();
                for d in disk_metrics.values() {
                    let is_active = if d.letter == 'C' || !app_cfg.disk_hide_when_idle {
                        true
                    } else {
                        d.last_active
                            .map(|la| now.duration_since(la).as_secs_f32() <= disk_inactive_timeout_secs)
                            .unwrap_or(false)
                    };
                    if is_active {
                        let read_int = (d.current_read_mbps + 0.5) as i32;
                        let write_int = (d.current_write_mbps + 0.5) as i32;
                        disks_sorted.push(DiskMetricSnap {
                            letter: d.letter,
                            read_mbps_wide: if app_cfg.hide_zeros && read_int == 0 { [0; 8] } else { int_to_wide_fixed(read_int) },
                            write_mbps_wide: if app_cfg.hide_zeros && write_int == 0 { [0; 8] } else { int_to_wide_fixed(write_int) },
                            read_history: d.read_history,
                            write_history: d.write_history,
                        });
                    }
                }
                disks_sorted.sort_unstable_by(|a, b| {
                    if a.letter == 'C' {
                        std::cmp::Ordering::Less
                    } else if b.letter == 'C' {
                        std::cmp::Ordering::Greater
                    } else {
                        a.letter.cmp(&b.letter)
                    }
                });

                let top_val = if app_cfg.net_show_current_mb { net_down_mbps } else { net_scale_max(&net_down_hist, &net_up_hist) };
                format_float_to_wide(
                    top_val,
                    if top_val < 0.1 { 2 } else if top_val < 10.0 { 1 } else { 0 },
                    &mut net_scale_max_wide,
                );

                let bottom_val = if app_cfg.net_show_current_mb { net_up_mbps } else { 0.01 };
                format_float_to_wide(
                    bottom_val,
                    if bottom_val < 0.1 { 2 } else if bottom_val < 10.0 { 1 } else { 0 },
                    &mut net_scale_min_wide,
                );

                format_float_to_wide(used_ram, 1, &mut ram_used_wide);
                format_float_to_wide(raw_vram_bytes * BYTES_TO_GB, 1, &mut gpu_vram_used_wide);

                let gpu_vram_pct = if total_vram_bytes > 0.0 {
                    (raw_vram_bytes / total_vram_bytes * 100.0).clamp(0.0, 100.0)
                } else {
                    0.0
                };
                
                cpu_temp_wide.clear();
                if app_cfg.show_temperatures && app_cfg.show_cpu_temp && cpu_temp > 0.0 {
                    format_temp_to_wide(cpu_temp, &app_cfg.temp_suffix, &mut cpu_temp_wide);
                }
                gpu_temp_wide.clear();
                if app_cfg.show_temperatures && app_cfg.show_gpu_temp && gpu_temp > 0.0 {
                    format_temp_to_wide(gpu_temp, &app_cfg.temp_suffix, &mut gpu_temp_wide);
                }

                let snapshot = MetricsSnapshot {
                    ram_used_wide: ram_used_wide.clone(),
                    ram_pct: used_ram / total_ram * 100.0,
                    cpu_pct: smoothed_cpu,
                    gpu_pct: smoothed_gpu,
                    gpu_vram_used_wide: gpu_vram_used_wide.clone(),
                    gpu_vram_pct,
                    cpu_temp_wide: cpu_temp_wide.clone(),
                    gpu_temp_wide: gpu_temp_wide.clone(),
                    net_down_history: net_down_hist.clone(),
                    net_up_history: net_up_hist,
                    net_scale_max_wide: net_scale_max_wide.clone(),
                    net_scale_min_wide: net_scale_min_wide.clone(),
                    top_processes: proc_list.clone(),
                    disks: disks_sorted.clone(),
                    total_disk_count: disk_metrics.len(),
                };

                if tx.send(snapshot).is_err() {
                    break;
                }
                unsafe {
                    let _ = PostMessageW(Some(HWND(hwnd_val as *mut _)), WM_USER_NEW_FRAME, WPARAM(0), LPARAM(0));
                }
            }

            let work_duration = loop_start.elapsed().as_secs_f32();
            let target_sleep = (POLL_INTERVAL_SECS - work_duration).max(0.01);
            std::thread::sleep(std::time::Duration::from_secs_f32(target_sleep));
        }
    });
}

// ============================================================================
//                             GDI+ DRAWING
// ============================================================================

// ============================================================================
//                           FONT & TEXT HELPERS
// ============================================================================

/// Draw text using the title font (section headers).

unsafe fn measure_str_raw_wide(
    graphics: *mut GpGraphics,
    text: &[u16],
    font: *mut GpFont,
    h: f32,
) -> f32 {
    let len = if text.last() == Some(&0) { text.len() - 1 } else { text.len() };
    if len == 0 { return 0.0; }
    let layout = RectF { X: 0.0, Y: 0.0, Width: 4096.0, Height: h };
    let mut b = RectF { X: 0.0, Y: 0.0, Width: 0.0, Height: 0.0 };
    GdipMeasureString(
        graphics, PCWSTR(text.as_ptr()), len as i32,
        font, &layout, null(), &mut b,
        null_mut(), null_mut(),
    );
    b.Width
}

fn split_at_dot_wide(s: &[u16]) -> (&[u16], &[u16]) {
    let len = if s.last() == Some(&0) { s.len() - 1 } else { s.len() };
    let s = &s[..len];
    if let Some(idx) = s.iter().position(|&c| c == b'.' as u16) {
        s.split_at(idx)
    } else {
        (s, &[])
    }
}

unsafe fn draw_title(
    graphics: *mut GpGraphics,
    res: &mut RenderResources,
    text: &[u16],
    x: f32,
    y: f32,
    w: f32,
) {
    if text.is_empty() || text[0] == 0 { return; }
    let len = if text.last() == Some(&0) { text.len() - 1 } else { text.len() };
    let rect = RectF { X: x, Y: y, Width: w, Height: res.title_h * 1.5 };
    GdipDrawString(graphics, PCWSTR(text.as_ptr()), len as i32, res.font_title, &rect, null(), res.brush_text as _);
}

unsafe fn draw_body(
    graphics: *mut GpGraphics,
    res: &mut RenderResources,
    text: &[u16],
    x: f32,
    y: f32,
    w: f32,
) {
    if text.is_empty() || text[0] == 0 { return; }
    let len = if text.last() == Some(&0) { text.len() - 1 } else { text.len() };
    let mut draw_w = w;
    let mut format = null();
    if TRUNCATE_LONG_PROCESS_NAMES && draw_w > MAX_PROCESS_NAME_WIDTH {
        draw_w = MAX_PROCESS_NAME_WIDTH;
        format = res.format_ellipsis;
    } else if TRUNCATE_LONG_PROCESS_NAMES {
        format = res.format_ellipsis;
    }

    let rect = RectF { X: x, Y: y, Width: draw_w, Height: res.body_h * 2.0 };
    GdipDrawString(graphics, PCWSTR(text.as_ptr()), len as i32, res.font_body, &rect, format, res.brush_text as _);
}

unsafe fn measure_str_w_fast_wide(
    graphics: *mut GpGraphics,
    text: &[u16],
    font: *mut GpFont,
    h: f32,
    res: &mut RenderResources,
) -> f32 {
    let len = if text.last() == Some(&0) { text.len() - 1 } else { text.len() };
    if len == 0 { return 0.0; }
    if len == 1 {
        let b = text[0];
        if b >= b'0' as u16 && b <= b'9' as u16 {
            let idx = (b - b'0' as u16) as usize;
            if font == res.font_body { return res.digit_body_w[idx]; }
            if font == res.font_value { return res.digit_value_w[idx]; }
        }
    }
    let layout = RectF { X: 0.0, Y: 0.0, Width: 4096.0, Height: h };
    let mut b = RectF { X: 0.0, Y: 0.0, Width: 0.0, Height: 0.0 };
    GdipMeasureString(
        graphics, PCWSTR(text.as_ptr()), len as i32,
        font, &layout, null(), &mut b,
        null_mut(), null_mut(),
    );
    b.Width
}

unsafe fn draw_dotj_body(
    graphics: *mut GpGraphics,
    res: &mut RenderResources,
    text: &[u16],
    dot_x: f32,
    y: f32,
) {
    if text.is_empty() || text[0] == 0 { return; }
    let len = if text.last() == Some(&0) { text.len() - 1 } else { text.len() };
    let h = res.body_h * 1.5;
    let (int_s, _) = split_at_dot_wide(text);
    let int_w = measure_str_w_fast_wide(graphics, int_s, res.font_body, h, res);
    let text_x = dot_x - int_w;
    let rect = RectF { X: text_x, Y: y, Width: int_w + 100.0, Height: h };
    GdipDrawString(graphics, PCWSTR(text.as_ptr()), len as i32, res.font_body, &rect, null(), res.brush_text as _);
}

unsafe fn draw_dotj_value(
    graphics: *mut GpGraphics,
    res: &mut RenderResources,
    text: &[u16],
    dot_x: f32,
    y: f32,
) {
    if text.is_empty() || text[0] == 0 { return; }
    let len = if text.last() == Some(&0) { text.len() - 1 } else { text.len() };
    let h = res.title_h * 1.5;
    let (int_s, _) = split_at_dot_wide(text);
    let int_w = measure_str_w_fast_wide(graphics, int_s, res.font_value, h, res);
    let text_x = dot_x - int_w;
    let rect = RectF { X: text_x, Y: y, Width: int_w + 200.0, Height: h };
    GdipDrawString(graphics, PCWSTR(text.as_ptr()), len as i32, res.font_value, &rect, null(), res.brush_text as _);
}

// ============================================================================
//                              BAR & GRAPH DRAWING
// ============================================================================


/// Draw active solid white bar fill over pre-rendered background bar
unsafe fn draw_active_bar(
    graphics: *mut GpGraphics,
    res: &mut RenderResources,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    pct: f32,
) {
    let clamped = pct.clamp(0.0, 100.0);
    let fill_w = (w * (clamped / 100.0)).round();
    if fill_w > 0.0 {
        GdipFillRectangle(graphics, res.brush_meter as _, x, y, fill_w, h);
    }
}

/// Draw a main section meter row: value number + active bar fill.
/// Section titles are pre-rendered in static background layer.
unsafe fn draw_meter_row(
    graphics: *mut GpGraphics,
    res: &mut RenderResources,
    y: i32,
    val_wide: &[u16],
    pct: f32,
    num_y_offset: f32,
    bar_y_offset: f32,
    app_cfg: &crate::config::AppConfig,
) {
    if !app_cfg.hide_zeros || pct >= 0.5 {
        draw_dotj_value(graphics, res, val_wide, res.layout.value_int_x(), y as f32 + res.layout.large_num_y_offset + num_y_offset);
    }

    let bar_x = res.layout.bar_start_x() as f32;
    let bar_y = ((y as f32 + res.meter_bar_y_offset + res.layout.large_bar_y_offset + bar_y_offset) + 0.5) as i32 as f32;
    draw_active_bar(graphics, res, bar_x, bar_y, res.layout.section_bar_w as f32, res.layout.bar_h as f32, pct);
}

/// A process sub-row: indented name + dot-justified % number + mini bar.
/// Bar left-edge aligns exactly with bar_start_x().
unsafe fn draw_process_row(
    graphics: *mut GpGraphics,
    res: &mut RenderResources,
    y: i32,
    name_wide: &[u16],
    cpu: f32,
    app_cfg: &crate::config::AppConfig,
) {
    let name_x = (res.layout.margin_left + res.layout.process_indent) as f32;
    let num_right = res.layout.bar_start_x();
    let name_w = (num_right - res.layout.process_indent - res.layout.margin_left - res.layout.proc_num_w) as f32;
    draw_body(graphics, res, name_wide, name_x, y as f32, name_w);

    let cpu_int = (cpu + 0.5) as i32;
    if cpu_int != 0 || !app_cfg.hide_zeros {
        let slice = get_pct_slice(cpu);
        draw_dotj_body(graphics, res, slice, res.layout.little_num_int_x(), y as f32);
    }

    let bar_x = res.layout.bar_start_x() as f32;
    let bar_y = ((y as f32 + res.proc_bar_y_offset) + 0.5) as i32 as f32;
    let scaled_pct = (cpu / PROCESS_BAR_SCALE * 100.0).clamp(0.0, 100.0);
    draw_active_bar(graphics, res, bar_x, bar_y, res.layout.mini_bar_w as f32, res.layout.bar_h as f32, scaled_pct);
}

/// Draw network trend line graphs batched in a single GdipDrawLines polyline call (1 call instead of 119 calls).
unsafe fn draw_line_graph(
    graphics: *mut GpGraphics,
    pen: *mut GpPen,
    history: &History120,
    graph_left: f32,
    graph_w: f32,
    mid_y: f32,
    half_h: f32,
    scale_max: f32,
    mirror_down: bool,
) {
    let step_x = graph_w / (GRAPH_HISTORY_SAMPLES - 1) as f32;
    let mut points = [PointF { X: 0.0, Y: 0.0 }; GRAPH_HISTORY_SAMPLES];
    for i in 0..GRAPH_HISTORY_SAMPLES {
        let x = graph_left + i as f32 * step_x;
        let h = scale_f32(history.get(i), scale_max, half_h);
        let y = if mirror_down { mid_y + h } else { mid_y - h };
        points[i] = PointF { X: x, Y: y };
    }
    GdipDrawLines(graphics, pen, points.as_ptr(), GRAPH_HISTORY_SAMPLES as i32);
}

unsafe fn draw_column_meter(
    graphics: *mut GpGraphics,
    pen: *mut GpPen,
    history: &History120,
    graph_left: f32,
    graph_w: f32,
    base_y: f32,
    max_h: f32,
    scale_max: f32,
    mirror_down: bool,
) {
    let step_x = graph_w / GRAPH_HISTORY_SAMPLES as f32;
    for i in 0..GRAPH_HISTORY_SAMPLES {
        let x = graph_left + i as f32 * step_x + step_x * 0.5;
        let h = scale_f32(history.get(i), scale_max, max_h);
        if h >= 0.5 {
            if mirror_down {
                GdipDrawLine(graphics, pen, x, base_y, x, base_y + h);
            } else {
                GdipDrawLine(graphics, pen, x, base_y, x, base_y - h);
            }
        }
    }
}

// ============================================================================
//                               MAIN RENDERER
// ============================================================================
//
// MAX-QUALITY OFFLINE PIPELINE (PRODUCTION-GRADE ARCHITECTURE)
// ------------------------------------------------------------
// This rendering loop is hyper-optimized for minimal GUI thread latency.
// Techniques employed:
// 1. Zero-Work Rendering: The background thread (`MetricsSnapshot`) fully pre-formats 
//    and pre-encodes strings into `Vec<u16>` UTF-16 memory buffers natively compatible with Win32.
// 2. Static 0-100 LUT (Look-Up Table): Percentage numbers (CPU, GPU, Process %s) are resolved 
//    instantly via an `O(1)` memory pointer swap to a statically initialized Look-Up Table (`PCT_LUT`).
// 3. Precalculated Constant Coordinate Layout: All structural y-advances (e.g., `proc_row_h`, 
//    `net_total_h`, `bar_y_offset`) are computed mathematically exactly once at launch into `RenderResources`.
// 4. Batch Hardware Rendering: Eliminates all runtime string allocations, `write!` formatting, 
//    and float `.round()` logic from the 30 FPS hot-path.
//
// As a result, this loop only consists of struct reads, math addition, and native hardware-accelerated GDI calls.

unsafe fn render_gadget(
    hdc_mem: HDC,
    graphics: *mut GpGraphics,
    res: &mut RenderResources,
    snapshot: &MetricsSnapshot,
    current_widget_height: i32,
) {
    let app_cfg = res.app_cfg.clone();
    let layout = res.layout;
    // 1. Instant 0.005ms BitBlt copy of static background layer (titles + 100% background bars)
    let _ = BitBlt(
        hdc_mem,
        0,
        0,
        layout.widget_width,
        current_widget_height,
        Some(res.hdc_bg),
        0,
        0,
        SRCCOPY,
    );

    GdipSetSmoothingMode(graphics, SmoothingModeAntiAlias);
    GdipSetTextRenderingHint(graphics, TextRenderingHintAntiAlias);
    
    let mut y = res.layout.padding_top;
    
    // ------------------------------------------------------------------ RAM
    draw_meter_row(graphics, res, y, &snapshot.ram_used_wide, snapshot.ram_pct, 0.0, 0.0, &app_cfg);
    y += res.layout.adv_ram_cpu;

    // ------------------------------------------------------------------ CPU
    let cpu_slice = get_pct_slice(snapshot.cpu_pct);
    draw_meter_row(graphics, res, y, cpu_slice, snapshot.cpu_pct, 0.0, 0.0, &app_cfg);
    
    if snapshot.cpu_temp_wide.len() > 0 {
        let tx = res.layout.value_int_x() + res.temp_x_offset;
        let ty = y as f32 + res.temp_y_offset;
        draw_body(graphics, res, &snapshot.cpu_temp_wide, tx, ty, res.layout.label_col_w as f32);
    }
    
    y += res.layout.adv_cpu_proc;

    // Process sub-rows
    let mut proc_y = y;
    for i in 0..TOP_PROCESS_COUNT {
        let (cpu, name) = if i < snapshot.top_processes.len() {
            (snapshot.top_processes[i].cpu_pct, snapshot.top_processes[i].name_wide.as_slice())
        } else {
            (0.0, &[] as &[u16])
        };
        
        let is_idle = (cpu + 0.5) as i32 == 0;
        let show_row = !(app_cfg.process_hide_when_idle && is_idle);
        let name_visible = show_row && name.len() > 0;
        
        if app_cfg.process_hide_bars_when_idle {
            if name_visible != res.proc_bg_visible[i] {
                res.proc_bg_visible[i] = name_visible;
                if name_visible {
                    let bar_y = ((proc_y as f32 + res.proc_bar_y_offset) + 0.5) as i32 as f32;
                    GdipFillRectangle(res.graphics_bg, res.brush_meter_up as _, res.layout.bar_start_x() as f32, bar_y, res.layout.mini_bar_w as f32, res.layout.bar_h as f32);
                    GdipFillRectangle(graphics, res.brush_meter_up as _, res.layout.bar_start_x() as f32, bar_y, res.layout.mini_bar_w as f32, res.layout.bar_h as f32);
                } else {
                    clear_bg_rect(res.graphics_bg, proc_y, res.proc_row_h, res.layout.widget_width);
                    clear_bg_rect(graphics, proc_y, res.proc_row_h, res.layout.widget_width);
                }
            }
        }
        
        if show_row && name.len() > 0 {
            draw_process_row(graphics, res, proc_y, name, cpu, &app_cfg);
        }
        proc_y += res.proc_row_h;
    }
    y += res.net_total_h;
    y += res.layout.adv_proc_vram;

    // ------------------------------------------------------------------ VRAM
    draw_meter_row(graphics, res, y, &snapshot.gpu_vram_used_wide, snapshot.gpu_vram_pct, 0.0, 0.0, &app_cfg);
    y += res.layout.adv_vram_gpu;

    // ------------------------------------------------------------------ GPU
    let gpu_should_show = !app_cfg.gpu_hide_when_idle || app_cfg.gpu_always_visible || (snapshot.gpu_pct + 0.5) as i32 > 0;
    if gpu_should_show && snapshot.gpu_temp_wide.len() > 0 {
        let tx = res.layout.value_int_x() + res.temp_x_offset;
        let ty = y as f32 + res.temp_y_offset;
        draw_body(graphics, res, &snapshot.gpu_temp_wide, tx, ty, res.layout.label_col_w as f32);
    }
    if gpu_should_show != res.gpu_bg_visible {
        res.gpu_bg_visible = gpu_should_show;
        if gpu_should_show {
            let gpu_title = [b'G' as u16, b'P' as u16, b'U' as u16, 0];
            let bar_y = ((y as f32 + res.meter_bar_y_offset + res.layout.large_bar_y_offset) + 0.5) as i32 as f32;
            
            draw_title(res.graphics_bg, res, &gpu_title, res.layout.margin_left as f32, y as f32, res.layout.label_col_w as f32);
            GdipFillRectangle(res.graphics_bg, res.brush_meter_up as _, res.layout.bar_start_x() as f32, bar_y, res.layout.section_bar_w as f32, res.layout.bar_h as f32);
            
            draw_title(graphics, res, &gpu_title, res.layout.margin_left as f32, y as f32, res.layout.label_col_w as f32);
            GdipFillRectangle(graphics, res.brush_meter_up as _, res.layout.bar_start_x() as f32, bar_y, res.layout.section_bar_w as f32, res.layout.bar_h as f32);
        } else {
            clear_bg_rect(res.graphics_bg, y, res.th, res.layout.widget_width);
            clear_bg_rect(graphics, y, res.th, res.layout.widget_width);
        }
    }
    
    if gpu_should_show {
        let gpu_slice = get_pct_slice(snapshot.gpu_pct);
        draw_meter_row(graphics, res, y, gpu_slice, snapshot.gpu_pct, 0.0, 0.0, &app_cfg);
    }
    y += res.layout.adv_gpu_net;

    // ---------------------------------------------------------------- Network
    // Network section header is pre-rendered.
    y += res.layout.adv_net_graph;
    let graph_top = y as f32;
    let graph_left = res.layout.bar_start_x() as f32;
    let graph_w = res.layout.bar_w as f32;

    // Network graph height = same total height as process block (4 rows),
    // giving visual rhythm parity with the section above.
    let scale_max      = net_scale_max(&snapshot.net_down_history, &snapshot.net_up_history);
    let total_net_h    = (TOP_PROCESS_COUNT as i32 * res.proc_row_h) as f32;
    let dl_h           = total_net_h * (4.0 / 5.0); // download occupies top 4/5
    let ul_h           = total_net_h * (1.0 / 5.0); // upload occupies bottom 1/5
    let mid_y          = graph_top + dl_h;   // divider between DL and UL

    // Axis scale labels: integer part right-aligns at little_num_int_x()
    // Only top DL scale number gets +3px offset (plus requested +3)
    draw_dotj_body(graphics, res, &snapshot.net_scale_max_wide,
        res.layout.little_num_int_x(), graph_top + res.layout.scale_6);

    let ul_y = graph_top + res.layout.scale_6 as f32 + 3.0 * res.proc_row_h as f32;
    draw_dotj_body(graphics, res, &snapshot.net_scale_min_wide,
        res.layout.little_num_int_x(), ul_y);

    // Download: grows upward from mid_y
    draw_line_graph(graphics, res.pen_meter, &snapshot.net_down_history,
        graph_left, graph_w, mid_y, dl_h, scale_max, false);

    // Upload: grows downward starting from just under DL line (+1px)
    let ul_mid_y = mid_y + res.layout.scale_2;
    draw_line_graph(graphics, res.pen_meter_up, &snapshot.net_up_history,
        graph_left, graph_w, ul_mid_y, ul_h, scale_max, true);

    // Clear whichever is lower: the graph bottom or the UL text bottom
    let graph_bottom = graph_top as i32 + res.net_total_h;
    let ul_text_bottom = ul_y as i32 + res.body_h as i32;
    y = graph_bottom.max(ul_text_bottom);
    y += res.layout.adv_graph_disk;

    // ------------------------------------------------------------------ Disk
    let disk_y = y;
    if !snapshot.disks.is_empty() {
        let disk_title = [b'D' as u16, b'i' as u16, b's' as u16, b'k' as u16, 0];
        draw_title(graphics, res, &disk_title, res.layout.margin_left as f32, disk_y as f32, (res.layout.widget_width - res.layout.margin_left - res.layout.padding_right) as f32);
        y += res.layout.adv_disk_c;

        let disk_row_h = 2 * res.proc_row_h + res.layout.scale_2 as i32;

        for disk in &snapshot.disks {
            // Letter: indented (same indent as process names)
            let letter_wide = [disk.letter as u16, 0];
            let letter_y = y + (disk_row_h - res.body_h as i32) / 2;
            draw_body(graphics, res, &letter_wide,
                (res.layout.margin_left + res.layout.process_indent) as f32, letter_y as f32,
                (res.layout.bar_start_x() - res.layout.margin_left - res.layout.process_indent - res.layout.proc_num_w) as f32);

            // Read MB/s (top tiny_num)
            let read_y = y as f32 + res.disk_num_y_offset;
            draw_dotj_body(graphics, res, &disk.read_mbps_wide, res.layout.little_num_int_x(), read_y);
            // Write MB/s (bottom tiny_num)
            let write_y = (y + disk_row_h - res.proc_row_h) as f32 + res.disk_num_y_offset;
            draw_dotj_body(graphics, res, &disk.write_mbps_wide, res.layout.little_num_int_x(), write_y);

            let read_max = disk_scale_max(&disk.read_history);
            let write_max = disk_scale_max(&disk.write_history);
            let scale_max = read_max.max(write_max);
            
            let mid_y = y as f32 + disk_row_h as f32 * DISK_GRAPH_READ_RATIO;
            let read_h = disk_row_h as f32 * DISK_GRAPH_READ_RATIO - res.layout.scale_2 as f32;
            let write_h = disk_row_h as f32 * DISK_GRAPH_WRITE_RATIO;

            draw_column_meter(graphics, res.pen_meter, &disk.read_history, graph_left, graph_w, mid_y, read_h, scale_max, false);
            draw_column_meter(graphics, res.pen_meter_up, &disk.write_history, graph_left, graph_w, mid_y + res.layout.scale_2 as f32, write_h, scale_max, true);

            y += disk_row_h;
        }
    }
}

// ============================================================================
//                               MAIN ENTRYPOINT
// ============================================================================

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if let Some(&taskbar_msg) = WM_TASKBAR_CREATED.get() {
        if msg == taskbar_msg && taskbar_msg != 0 {
            let app_cfg = crate::config::CONFIG.read().unwrap();
            if app_cfg.enable_tray {
                IS_TRAY_ACTIVE.store(false, Ordering::Relaxed);
                add_tray_icon(hwnd);
            }
            return LRESULT(0);
        }
    }

    match msg {
        WM_GETICON => {
            let icon_type = wparam.0;
            let instance = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
            let icon = if icon_type == ICON_SMALL as usize {
                LoadImageW(
                    Some(HINSTANCE(instance.0)),
                    PCWSTR(1 as *const u16),
                    IMAGE_ICON,
                    GetSystemMetrics(SM_CXSMICON),
                    GetSystemMetrics(SM_CYSMICON),
                    LR_DEFAULTCOLOR | LR_SHARED,
                ).map(|h| h.0 as isize).unwrap_or(0)
            } else {
                LoadImageW(
                    Some(HINSTANCE(instance.0)),
                    PCWSTR(1 as *const u16),
                    IMAGE_ICON,
                    GetSystemMetrics(SM_CXICON),
                    GetSystemMetrics(SM_CYICON),
                    LR_DEFAULTCOLOR | LR_SHARED,
                ).map(|h| h.0 as isize).unwrap_or(0)
            };
            if icon != 0 {
                LRESULT(icon)
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_DPICHANGED => {
            let new_dpi = (wparam.0 & 0xFFFF) as u32;
            let prc_new_window = lparam.0 as *const RECT;
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    Option::<HWND>::None,
                    (*prc_new_window).left,
                    (*prc_new_window).top,
                    (*prc_new_window).right - (*prc_new_window).left,
                    (*prc_new_window).bottom - (*prc_new_window).top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            let _ = PostMessageW(Some(hwnd), WM_USER_DPI_CHANGED, WPARAM(new_dpi as usize), LPARAM(0));
            LRESULT(0)
        }
        WM_POWERBROADCAST => {
            let event = wparam.0;
            if event == PBT_APMSUSPEND {
                IS_SYSTEM_SUSPENDED.store(true, Ordering::Relaxed);
            } else if event == PBT_APMRESUMEAUTOMATIC || event == PBT_APMRESUMESUSPEND {
                IS_SYSTEM_SUSPENDED.store(false, Ordering::Relaxed);
            }
            LRESULT(1)
        }
        WM_USER_TRAY_ICON => {
            if lparam.0 == WM_LBUTTONDBLCLK as isize {
                PostQuitMessage(0);
            } else if lparam.0 == WM_RBUTTONUP as isize {
                let mut pt = POINT::default();
                let _ = windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pt);
                if let std::result::Result::Ok(hmenu) = CreatePopupMenu() {
                    let _ = InsertMenuW(hmenu, 0, MF_BYPOSITION | MF_STRING, 1001, w!("Open Config"));
                    let _ = InsertMenuW(hmenu, 1, MF_BYPOSITION | MF_STRING, 1002, w!("Reload Config"));
                    let _ = InsertMenuW(hmenu, 2, MF_BYPOSITION | MF_SEPARATOR, 0, PCWSTR::null());
                    let _ = InsertMenuW(hmenu, 3, MF_BYPOSITION | MF_STRING, 1003, w!("Exit"));
                    
                    let _ = SetForegroundWindow(hwnd);
                    let _ = TrackPopupMenu(hmenu, TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, Some(0), hwnd, None);
                    let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
                    let _ = DestroyMenu(hmenu);
                }
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let wm_id = wparam.0 as u16;
            match wm_id {
                1001 => { // Open Config
                    let ini_p = crate::config::get_ini_path();
                    let path = to_wide(ini_p.to_str().unwrap_or(""));
                    // Preserved for alternate shells / power user builds (don't delete):
                    // let _ = ShellExecuteW(None, w!("open"), PCWSTR(path.as_ptr()), None, None, windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL);
                    let _ = ShellExecuteW(None, w!("open"), w!("notepad.exe"), PCWSTR(path.as_ptr()), None, windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL);
                },
                1002 => { // Reload Config
                    crate::config::load_or_create_ini();
                    let cfg = crate::config::CONFIG.read().unwrap();
                    update_startup_registry(cfg.run_on_startup);
                    CONFIG_RELOAD_REQUESTED.store(true, Ordering::Relaxed);
                    let _ = PostMessageW(Some(hwnd), WM_USER_CONFIG_RELOADED, WPARAM(0), LPARAM(0));
                },
                1003 => { // Exit
                    PostQuitMessage(0);
                },
                _ => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let app_cfg = crate::config::CONFIG.read().unwrap();
            if app_cfg.enable_tray {
                remove_tray_icon(hwnd);
            }
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}


use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
static GLOBAL_HWND: AtomicIsize = AtomicIsize::new(0);
static IS_TRAY_ACTIVE: AtomicBool = AtomicBool::new(false);
static WM_TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();

unsafe extern "system" fn ctrl_handler(_ctrl_type: u32) -> windows::core::BOOL {
    let hwnd_val = GLOBAL_HWND.load(Ordering::Relaxed);
    if hwnd_val != 0 {
        let hwnd = windows::Win32::Foundation::HWND(hwnd_val as _);
        remove_tray_icon(hwnd);
    }
    windows::core::BOOL(0)
}

fn calculate_required_height(res: &RenderResources, snapshot: &MetricsSnapshot) -> i32 {
    // 523px is the exact bottom-most drawn pixel for a 1-drive layout (at 100% scale).
    let tight_base_end_y = res.layout.widget_height - 17; // 540 - 17 = 523
    let disk_row_h = 2 * res.proc_row_h + res.layout.scale_2 as i32;
    
    let total_disks = snapshot.total_disk_count.max(1) as i32;
    let end_y = tight_base_end_y + (total_disks - 1) * disk_row_h;
    
    // Add identical padding to the bottom as we have on the top (11px at 100% scale)
    end_y + res.layout.padding_top
}

fn update_startup_registry(enable: bool) {
    unsafe {
        let app_name = to_wide(&crate::config::get_identity().exe_stem);
        
        let mut hkey = windows::Win32::System::Registry::HKEY::default();
        let res = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run"),
            Some(0),
            KEY_SET_VALUE | KEY_QUERY_VALUE,
            &mut hkey,
        );
        
        if res.is_ok() {
            if enable {
                let mut path = [0u16; 1024];
                let len = GetModuleFileNameW(None, &mut path);
                if len > 0 {
                    let mut cmd = [0u16; 1030];
                    cmd[0] = b'"' as u16;
                    cmd[1..1 + len as usize].copy_from_slice(&path[..len as usize]);
                    cmd[1 + len as usize] = b'"' as u16;
                    cmd[2 + len as usize] = 0;
                    
                    let total_u16_len = 2 + len as usize;
                    let cmd_bytes = std::slice::from_raw_parts(cmd.as_ptr() as *const u8, total_u16_len * 2);
                    
                    let mut current_type = REG_VALUE_TYPE(0);
                    let mut current_data = [0u8; 2048];
                    let mut data_len = current_data.len() as u32;

                    let query_res = RegQueryValueExW(
                        hkey,
                        PCWSTR(app_name.as_ptr()),
                        None,
                        Some(&mut current_type),
                        Some(current_data.as_mut_ptr()),
                        Some(&mut data_len),
                    );

                    let needs_update = if query_res.is_ok() {
                        current_type != REG_SZ || &current_data[..data_len as usize] != cmd_bytes
                    } else {
                        true
                    };

                    if needs_update {
                        let _ = RegSetValueExW(
                            hkey,
                            PCWSTR(app_name.as_ptr()),
                            Some(0),
                            REG_SZ,
                            Some(cmd_bytes),
                        );
                    }
                }
            } else {
                let _ = RegDeleteValueW(hkey, PCWSTR(app_name.as_ptr()));
            }
            let _ = RegCloseKey(hkey);
        }
    }
}

fn enforce_single_instance() {
    let mut sys = System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    
    let current_pid = std::process::id();
    let identity = crate::config::get_identity();
    for (pid, process) in sys.processes() {
        if pid.as_u32() != current_pid {
            if let Some(exe_path) = process.exe() {
                if exe_path == identity.exe_path {
                    process.kill();
                    continue;
                }
            }
            // Fallback: If path matching fails or is unavailable, match dynamically by process name
            let name = process.name().to_str().unwrap_or("");
            let name_clean = name.trim_end_matches(".exe");
            if name.eq_ignore_ascii_case(&identity.exe_name)
                || name.eq_ignore_ascii_case(&identity.exe_stem)
                || name_clean.eq_ignore_ascii_case(&identity.exe_stem)
            {
                process.kill();
            }
        }
    }
}

fn is_elevated() -> bool {
    let mut elevated = false;
    unsafe {
        let mut h_token = windows::Win32::Foundation::HANDLE::default();
        if windows::Win32::System::Threading::OpenProcessToken(
            windows::Win32::System::Threading::GetCurrentProcess(),
            windows::Win32::Security::TOKEN_QUERY,
            &mut h_token,
        ).is_ok() {
            let mut elevation = windows::Win32::Security::TOKEN_ELEVATION::default();
            let mut size = std::mem::size_of::<windows::Win32::Security::TOKEN_ELEVATION>() as u32;
            let _ = windows::Win32::Security::GetTokenInformation(
                h_token,
                windows::Win32::Security::TokenElevation,
                Some(&mut elevation as *mut _ as *mut _),
                size,
                &mut size,
            );
            elevated = elevation.TokenIsElevated != 0;
            let _ = windows::Win32::Foundation::CloseHandle(h_token);
        }
    }
    elevated
}

fn require_admin() {
    if is_elevated() {
        return;
    }

    let show_temps = crate::config::CONFIG.read().map(|c| c.show_temperatures).unwrap_or(false);
    if !show_temps {
        return; // Elevation only required when hardware temperatures are enabled
    }

    if let std::result::Result::Ok(exe_path) = std::env::current_exe() {
        if let Some(path_str) = exe_path.to_str() {
            let exe_path_wide = to_wide(path_str);
            let mut dir_wide = Vec::new();
            if let Some(parent) = exe_path.parent() {
                dir_wide = to_wide(parent.to_str().unwrap_or(""));
            }

            unsafe {
                let hinstance = windows::Win32::UI::Shell::ShellExecuteW(
                    None,
                    w!("runas"),
                    PCWSTR(exe_path_wide.as_ptr()),
                    PCWSTR::null(),
                    if dir_wide.len() > 1 { PCWSTR(dir_wide.as_ptr()) } else { PCWSTR::null() },
                    windows::Win32::UI::WindowsAndMessaging::SW_SHOW,
                );
                if (hinstance.0 as usize) > 32 {
                    std::process::exit(0);
                }
            }
        }
    }
}

fn main() {
    crate::config::load_or_create_ini();
    require_admin();
    enforce_single_instance();
    
    unsafe {
        let _ = SetConsoleCtrlHandler(Some(ctrl_handler), true);
    }
    
    // Set custom panic hook for manual tray icon cleanup
    std::panic::set_hook(Box::new(|info| {
        let hwnd_val = GLOBAL_HWND.load(Ordering::Relaxed);
        if hwnd_val != 0 {
            unsafe {
                let hwnd = windows::Win32::Foundation::HWND(hwnd_val as _);
                remove_tray_icon(hwnd);
            }
        }
        println!("Panic occurred: {:?}", info);
    }));

    {
        let cfg = crate::config::CONFIG.read().unwrap();
        update_startup_registry(cfg.run_on_startup);
    }
    
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let mut gdiplus_token: usize = 0;
        let mut input: GdiplusStartupInput = std::mem::zeroed();
        input.GdiplusVersion = 1;
        GdiplusStartup(&mut gdiplus_token, &input, null_mut());

        let monitors = get_active_monitors();
        let target_mon = monitors
            .get(crate::config::CONFIG.read().unwrap().target_monitor_index)
            .cloned()
            .unwrap_or_else(|| {
                let cx = GetSystemMetrics(SM_CXSCREEN);
                let cy = GetSystemMetrics(SM_CYSCREEN);
                MonitorInfo {
                    rect: RECT { left: 0, top: 0, right: cx, bottom: cy },
                    dpi: GetDpiForSystem(),
                }
            });

        let layout = DpiLayout::new(target_mon.dpi);
        let mut pos_x = target_mon.rect.right - layout.widget_width - layout.padding_right;
        let mut pos_y = target_mon.rect.top + layout.padding_top;
        #[cfg(debug_assertions)]
        eprintln!(
            "[Gadget] Target Monitor Rect: {:?}, DPI: {} (scale {:.2}), Gadget Position: ({}, {})",
            target_mon.rect, target_mon.dpi, layout.scale, pos_x, pos_y
        );

        let instance = GetModuleHandleW(PCWSTR::null()).unwrap();
        let identity = crate::config::get_identity();
        let class_name = to_wide(&identity.window_class);
        let title_name = to_wide(&identity.app_title);

        let msg_taskbar = RegisterWindowMessageW(w!("TaskbarCreated"));
        let _ = WM_TASKBAR_CREATED.set(msg_taskbar);

        let cx_icon = GetSystemMetrics(SM_CXICON);
        let cy_icon = GetSystemMetrics(SM_CYICON);
        let cx_smicon = GetSystemMetrics(SM_CXSMICON);
        let cy_smicon = GetSystemMetrics(SM_CYSMICON);

        let hicon_big: HICON = LoadImageW(
            Some(HINSTANCE(instance.0)),
            PCWSTR(1 as *const u16),
            IMAGE_ICON,
            cx_icon,
            cy_icon,
            LR_DEFAULTCOLOR | LR_SHARED,
        ).map(|h| HICON(h.0)).unwrap_or_default();

        let hicon_sm: HICON = LoadImageW(
            Some(HINSTANCE(instance.0)),
            PCWSTR(1 as *const u16),
            IMAGE_ICON,
            cx_smicon,
            cy_smicon,
            LR_DEFAULTCOLOR | LR_SHARED,
        ).map(|h| HICON(h.0)).unwrap_or_default();

        let wnd_class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: WNDCLASS_STYLES(0),
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance.into(),
            hIcon: hicon_big,
            hIconSm: hicon_sm,
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        RegisterClassExW(&wnd_class);

        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title_name.as_ptr()),
            WS_POPUP,
            pos_x,
            pos_y,
            layout.widget_width,
            layout.widget_height,
            Option::<HWND>::None,
            Option::<HMENU>::None,
            Some(instance.into()),
            None,
        )
        .unwrap();

        if !hicon_big.0.is_null() {
            let _ = SendMessageW(hwnd, WM_SETICON, Some(WPARAM(ICON_BIG as usize)), Some(LPARAM(hicon_big.0 as isize)));
        }
        if !hicon_sm.0.is_null() {
            let _ = SendMessageW(hwnd, WM_SETICON, Some(WPARAM(ICON_SMALL as usize)), Some(LPARAM(hicon_sm.0 as isize)));
        }

        let mut res = RenderResources::new(layout);

        let mut msg = MSG::default();
        let hdc_screen = GetDC(Option::<HWND>::None);
        let hdc_mem = CreateCompatibleDC(Some(hdc_screen));
        
        let mut current_widget_height = layout.widget_height;
        let mut hbitmap = CreateCompatibleBitmap(hdc_screen, layout.widget_width, layout.widget_height + 400);
        let mut old_bitmap = SelectObject(hdc_mem, HGDIOBJ::from(hbitmap));

        let mut graphics: *mut GpGraphics = null_mut();
        GdipCreateFromHDC(hdc_mem, &mut graphics);

        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };

        let (tx, rx) = mpsc::channel::<MetricsSnapshot>();
        start_metrics_collector(tx, hwnd.0 as isize);

        let refresh_timeout = Duration::from_secs_f32(RENDER_INTERVAL_SECS + 1.0);

        let mut last_snapshot: Option<MetricsSnapshot> = None;

        // Receive first snapshot immediately and render real data before showing window
        if let std::result::Result::Ok(first_snapshot) = rx.recv_timeout(refresh_timeout) {
            current_widget_height = calculate_required_height(&res, &first_snapshot);
            render_gadget(hdc_mem, graphics, &mut res, &first_snapshot, current_widget_height);
            let mut window_pos = POINT { x: pos_x, y: pos_y };
            let mut window_size = SIZE { cx: layout.widget_width, cy: current_widget_height };
            let mut ppt_src = POINT { x: 0, y: 0 };
            let res_ulw = UpdateLayeredWindow(
                hwnd,
                Some(hdc_screen),
                Some(&mut window_pos as *mut _ as *const _),
                Some(&mut window_size as *mut _ as *const _),
                Some(hdc_mem),
                Some(&mut ppt_src as *mut _ as *const _),
                COLORREF(0),
                Some(&blend as *const _),
                ULW_ALPHA,
            );
            if res_ulw.is_err() {
                #[cfg(debug_assertions)]
                eprintln!("[Gadget Error] Initial UpdateLayeredWindow failed: {:?}", res_ulw);
            }
            last_snapshot = Some(first_snapshot);
        }

        attach_to_desktop(hwnd);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let enable_tray = crate::config::CONFIG.read().unwrap().enable_tray;
        if enable_tray {
            add_tray_icon(hwnd);
        }

        loop {
            let ret = GetMessageW(&mut msg, Option::<HWND>::None, 0, 0);
            if ret.0 == 0 || ret.0 == -1 {
                #[cfg(debug_assertions)]
                eprintln!("[Gadget] Event loop exited. GetMessageW returned: {}", ret.0);
                break;
            }

            if msg.message == WM_USER_DPI_CHANGED {
                let new_dpi = msg.wParam.0 as u32;
                let _ = SelectObject(hdc_mem, old_bitmap);
                let _ = DeleteObject(HGDIOBJ::from(hbitmap));
                GdipDeleteGraphics(graphics);
                
                let new_layout = DpiLayout::new(new_dpi);
                let monitors = get_active_monitors();
                let target_mon = monitors
                    .get(crate::config::CONFIG.read().unwrap().target_monitor_index)
                    .cloned()
                    .unwrap_or_else(|| {
                        let cx = GetSystemMetrics(SM_CXSCREEN);
                        let cy = GetSystemMetrics(SM_CYSCREEN);
                        MonitorInfo {
                            rect: RECT { left: 0, top: 0, right: cx, bottom: cy },
                            dpi: new_dpi,
                        }
                    });
                pos_x = target_mon.rect.right - new_layout.widget_width - new_layout.padding_right;
                pos_y = target_mon.rect.top + new_layout.padding_top;
                res.destroy();
                res = RenderResources::new(new_layout);
                
                let tmp_hdc = GetDC(Option::<HWND>::None);
                hbitmap = CreateCompatibleBitmap(tmp_hdc, res.layout.widget_width, res.layout.widget_height + 400);
                ReleaseDC(Option::<HWND>::None, tmp_hdc);
                old_bitmap = SelectObject(hdc_mem, HGDIOBJ::from(hbitmap));
                GdipCreateFromHDC(hdc_mem, &mut graphics);

                if let Some(ref snapshot) = last_snapshot {
                    current_widget_height = calculate_required_height(&res, snapshot);
                    render_gadget(hdc_mem, graphics, &mut res, snapshot, current_widget_height);
                    let mut window_pos = POINT { x: pos_x, y: pos_y };
                    let mut window_size = SIZE { cx: res.layout.widget_width, cy: current_widget_height };
                    let mut ppt_src = POINT { x: 0, y: 0 };
                    let _ = UpdateLayeredWindow(
                        hwnd,
                        Some(hdc_screen),
                        Some(&mut window_pos as *mut _ as *const _),
                        Some(&mut window_size as *mut _ as *const _),
                        Some(hdc_mem),
                        Some(&mut ppt_src as *mut _ as *const _),
                        COLORREF(0),
                        Some(&blend as *const _),
                        ULW_ALPHA,
                    );
                }
            } else if msg.message == WM_USER_CONFIG_RELOADED {
                let app_cfg = crate::config::CONFIG.read().unwrap().clone();
                if app_cfg.enable_tray {
                    add_tray_icon(hwnd);
                } else {
                    remove_tray_icon(hwnd);
                }

                let _ = SelectObject(hdc_mem, old_bitmap);
                let _ = DeleteObject(HGDIOBJ::from(hbitmap));
                GdipDeleteGraphics(graphics);
                
                let target_mon_idx = app_cfg.target_monitor_index;
                let monitors = get_active_monitors();
                let target_mon = monitors.get(target_mon_idx).cloned().unwrap_or_else(|| {
                    let cx = GetSystemMetrics(SM_CXSCREEN);
                    let cy = GetSystemMetrics(SM_CYSCREEN);
                    MonitorInfo {
                        rect: RECT { left: 0, top: 0, right: cx, bottom: cy },
                        dpi: GetDpiForSystem(),
                    }
                });
                let new_layout = DpiLayout::new(target_mon.dpi);
                pos_x = target_mon.rect.right - new_layout.widget_width - new_layout.padding_right;
                pos_y = target_mon.rect.top + new_layout.padding_top;

                let _ = SetWindowPos(
                    hwnd,
                    Option::<HWND>::None,
                    pos_x,
                    pos_y,
                    new_layout.widget_width,
                    current_widget_height,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );

                res.destroy();
                res = RenderResources::new(new_layout);
                
                let tmp_hdc = GetDC(Option::<HWND>::None);
                hbitmap = CreateCompatibleBitmap(tmp_hdc, res.layout.widget_width, res.layout.widget_height + 400);
                ReleaseDC(Option::<HWND>::None, tmp_hdc);
                old_bitmap = SelectObject(hdc_mem, HGDIOBJ::from(hbitmap));
                GdipCreateFromHDC(hdc_mem, &mut graphics);

                if let Some(ref snapshot) = last_snapshot {
                    current_widget_height = calculate_required_height(&res, snapshot);
                    render_gadget(hdc_mem, graphics, &mut res, snapshot, current_widget_height);
                    let mut window_pos = POINT { x: pos_x, y: pos_y };
                    let mut window_size = SIZE { cx: res.layout.widget_width, cy: current_widget_height };
                    let mut ppt_src = POINT { x: 0, y: 0 };
                    let _ = UpdateLayeredWindow(
                        hwnd,
                        Some(hdc_screen),
                        Some(&mut window_pos as *mut _ as *const _),
                        Some(&mut window_size as *mut _ as *const _),
                        Some(hdc_mem),
                        Some(&mut ppt_src as *mut _ as *const _),
                        COLORREF(0),
                        Some(&blend as *const _),
                        ULW_ALPHA,
                    );
                }
            } else if msg.message == WM_USER_NEW_FRAME {
                while let std::result::Result::Ok(snapshot) = rx.try_recv() {
                    current_widget_height = calculate_required_height(&res, &snapshot);

                    render_gadget(hdc_mem, graphics, &mut res, &snapshot, current_widget_height);

                    let mut window_pos = POINT { x: pos_x, y: pos_y };
                    let mut window_size = SIZE { cx: res.layout.widget_width, cy: current_widget_height };
                    let mut ppt_src = POINT { x: 0, y: 0 };
                    let _ = UpdateLayeredWindow(
                        hwnd,
                        Some(hdc_screen),
                        Some(&mut window_pos as *mut _ as *const _),
                        Some(&mut window_size as *mut _ as *const _),
                        Some(hdc_mem),
                        Some(&mut ppt_src as *mut _ as *const _),
                        COLORREF(0),
                        Some(&blend as *const _),
                        ULW_ALPHA,
                    );
                    last_snapshot = Some(snapshot);
                }
            } else {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        GdipDeleteGraphics(graphics);
        let _ = SelectObject(hdc_mem, old_bitmap);
        let _ = DeleteObject(HGDIOBJ::from(hbitmap));
        let _ = DeleteDC(hdc_mem);
        ReleaseDC(Option::<HWND>::None, hdc_screen);
        res.destroy();
        GdiplusShutdown(gdiplus_token);
    }
}
