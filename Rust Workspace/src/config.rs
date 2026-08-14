use std::fs;
use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};

#[derive(Clone)]
pub struct AppConfig {
    pub gpu_hide_when_idle: bool,
    pub disk_hide_when_idle: bool,
    pub process_hide_when_idle: bool,
    pub process_hide_bars_when_idle: bool,
    pub show_temperatures: bool,
    pub hide_zeros: bool,
    pub cpu_taskmgr_percentage_logic: bool,
    pub gpu_always_visible: bool,
    pub net_show_current_mb: bool,
    pub show_cpu_temp: bool,
    pub show_gpu_temp: bool,
    pub temp_suffix: String,
    pub render_interval_secs: f32,
    pub ini_check_interval_secs: f32,
    pub target_monitor_index: usize,
    pub enable_tray: bool,
    pub run_on_startup: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            gpu_hide_when_idle: true,
            disk_hide_when_idle: true,
            process_hide_when_idle: true,
            process_hide_bars_when_idle: true,
            show_temperatures: true,
            hide_zeros: true,
            cpu_taskmgr_percentage_logic: false,
            gpu_always_visible: false,
            net_show_current_mb: false,
            show_cpu_temp: true,
            show_gpu_temp: true,
            temp_suffix: " °C".to_string(),
            render_interval_secs: 2.0,
            ini_check_interval_secs: 10.0,
            target_monitor_index: 0,
            enable_tray: true,
            run_on_startup: false,
        }
    }
}

pub static CONFIG: RwLock<AppConfig> = RwLock::new(AppConfig {
    gpu_hide_when_idle: false,
    disk_hide_when_idle: false,
    process_hide_when_idle: false,
    process_hide_bars_when_idle: false,
    show_temperatures: true,
    hide_zeros: false,
    cpu_taskmgr_percentage_logic: false,
    gpu_always_visible: false,
    net_show_current_mb: false,
    show_cpu_temp: true,
    show_gpu_temp: true,
    temp_suffix: String::new(),
    render_interval_secs: 2.0,
    ini_check_interval_secs: 10.0,
    target_monitor_index: 0,
    enable_tray: true,
    run_on_startup: false,
});

/// Dynamic application runtime identity derived from `current_exe()`.
pub struct AppIdentity {
    pub exe_path: PathBuf,
    pub exe_name: String,
    pub exe_stem: String,
    pub ini_path: PathBuf,
    pub app_title: String,
    pub window_class: String,
}

static IDENTITY: OnceLock<AppIdentity> = OnceLock::new();

pub fn get_identity() -> &'static AppIdentity {
    IDENTITY.get_or_init(|| {
        let exe_path = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("gadget.exe"));

        let exe_name = exe_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "gadget.exe".to_string());

        let exe_stem = exe_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "gadget".to_string());

        let ini_path = exe_path.with_extension("ini");
        let app_title = exe_stem.clone();
        let window_class = format!("{}Class", exe_stem);

        AppIdentity {
            exe_path,
            exe_name,
            exe_stem,
            ini_path,
            app_title,
            window_class,
        }
    })
}

pub fn get_ini_path() -> PathBuf {
    get_identity().ini_path.clone()
}

pub fn load_or_create_ini() -> AppConfig {
    let ini_path = get_ini_path();
    let mut current_config = AppConfig::default();
    let mut needs_write = false;

    let file_res = fs::read(&ini_path);
    if let std::result::Result::Ok(bytes) = file_res {
        let content = String::from_utf8_lossy(&bytes);

        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
                continue;
            }

            if let Some((key, val)) = line.split_once('=') {
                let k = key.trim().to_lowercase();
                let v = val.trim();
                let v_lower = v.to_lowercase();

                let parse_bool = |s: &str| -> Option<bool> {
                    match s {
                        "1" | "true" | "yes" | "on" => Some(true),
                        "0" | "false" | "no" | "off" => Some(false),
                        _ => None,
                    }
                };

                match k.as_str() {
                    "gpu_hide_when_idle" => {
                        if let Some(b) = parse_bool(&v_lower) { current_config.gpu_hide_when_idle = b; }
                    }
                    "disk_hide_when_idle" => {
                        if let Some(b) = parse_bool(&v_lower) { current_config.disk_hide_when_idle = b; }
                    }
                    "process_hide_when_idle" => {
                        if let Some(b) = parse_bool(&v_lower) { current_config.process_hide_when_idle = b; }
                    }
                    "process_hide_bars_when_idle" => {
                        if let Some(b) = parse_bool(&v_lower) { current_config.process_hide_bars_when_idle = b; }
                    }
                    "show_temperatures" => {
                        if let Some(b) = parse_bool(&v_lower) { current_config.show_temperatures = b; }
                    }
                    "hide_zeros" => {
                        if let Some(b) = parse_bool(&v_lower) { current_config.hide_zeros = b; }
                    }
                    "cpu_taskmgr_percentage_logic" => {
                        if let Some(b) = parse_bool(&v_lower) { current_config.cpu_taskmgr_percentage_logic = b; }
                    }
                    "gpu_always_visible" => {
                        if let Some(b) = parse_bool(&v_lower) { current_config.gpu_always_visible = b; }
                    }
                    "net_show_current_mb" => {
                        if let Some(b) = parse_bool(&v_lower) { current_config.net_show_current_mb = b; }
                    }
                    "show_cpu_temp" => {
                        if let Some(b) = parse_bool(&v_lower) { current_config.show_cpu_temp = b; }
                    }
                    "show_gpu_temp" => {
                        if let Some(b) = parse_bool(&v_lower) { current_config.show_gpu_temp = b; }
                    }
                    "temp_suffix" => {
                        let trimmed_quotes = v.trim_matches('"').trim_matches('\'');
                        current_config.temp_suffix = trimmed_quotes.to_string();
                    }
                    "render_interval_secs" => {
                        if let Ok(f) = v.parse::<f32>() { current_config.render_interval_secs = f.max(0.1); }
                    }
                    "ini_check_interval_secs" => {
                        if let Ok(f) = v.parse::<f32>() { current_config.ini_check_interval_secs = f.max(0.5); }
                    }
                    "target_monitor_index" => {
                        if let Ok(i) = v.parse::<usize>() { current_config.target_monitor_index = i; }
                    }
                    "enable_tray" => {
                        if let Some(b) = parse_bool(&v_lower) { current_config.enable_tray = b; }
                    }
                    "run_on_startup" => {
                        if let Some(b) = parse_bool(&v_lower) { current_config.run_on_startup = b; }
                    }
                    _ => {} // Ignore unknown keys
                }
            }
        }
    } else {
        needs_write = true; // File doesn't exist
    }

    if needs_write {
        let out = format!(
            "; {} Configuration\n\
            ; Visual clutter\n\
            GPU_HIDE_WHEN_IDLE={}\n\
            DISK_HIDE_WHEN_IDLE={}\n\
            PROCESS_HIDE_WHEN_IDLE={}\n\
            PROCESS_HIDE_BARS_WHEN_IDLE={}\n\
            SHOW_TEMPERATURES={}\n\
            HIDE_ZEROS={}\n\
            ; Different functionality\n\
            CPU_TASKMGR_PERCENTAGE_LOGIC={}\n\
            GPU_ALWAYS_VISIBLE={}\n\
            NET_SHOW_CURRENT_MB={}\n\
            SHOW_CPU_TEMP={}\n\
            SHOW_GPU_TEMP={}\n\
            TEMP_SUFFIX=\"{}\"\n\
            ; Core features\n\
            RENDER_INTERVAL_SECS={}\n\
            INI_CHECK_INTERVAL_SECS={}\n\
            ; Program features\n\
            TARGET_MONITOR_INDEX={}\n\
            ENABLE_TRAY={}\n\
            RUN_ON_STARTUP={}\n\
            ; Style decisions\n\
            ; ...\n\
            ; More in Rust source code: main.rs\n",
            get_identity().exe_stem,
            current_config.gpu_hide_when_idle,
            current_config.disk_hide_when_idle,
            current_config.process_hide_when_idle,
            current_config.process_hide_bars_when_idle,
            current_config.show_temperatures,
            current_config.hide_zeros,
            current_config.cpu_taskmgr_percentage_logic,
            current_config.gpu_always_visible,
            current_config.net_show_current_mb,
            current_config.show_cpu_temp,
            current_config.show_gpu_temp,
            current_config.temp_suffix,
            current_config.render_interval_secs,
            current_config.ini_check_interval_secs,
            current_config.target_monitor_index,
            current_config.enable_tray,
            current_config.run_on_startup
        );
        let _ = fs::write(&ini_path, out);
    }

    if let std::result::Result::Ok(mut lock) = CONFIG.write() {
        *lock = current_config.clone();
    }
    current_config
}
