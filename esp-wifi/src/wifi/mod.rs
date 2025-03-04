//! WiFi

pub(crate) mod os_adapter;
pub(crate) mod state;

use core::ptr::addr_of;
use core::time::Duration;
use core::{
    cell::{RefCell, RefMut},
    mem::MaybeUninit,
};

use portable_atomic::{AtomicUsize, Ordering};

use crate::common_adapter::*;
use crate::esp_wifi_result;
use crate::hal::macros::ram;
use crate::hal::peripheral::Peripheral;
use crate::hal::peripheral::PeripheralRef;
use crate::EspWifiInitialization;

use critical_section::{CriticalSection, Mutex};

use embedded_svc::wifi::{
    AccessPointConfiguration, AccessPointInfo, AuthMethod, ClientConfiguration, Configuration,
    Protocol, SecondaryChannel, Wifi,
};

use enumset::EnumSet;
use enumset::EnumSetType;
use num_derive::FromPrimitive;
use num_traits::FromPrimitive;

#[doc(hidden)]
pub use os_adapter::*;
pub use state::*;

#[cfg(feature = "smoltcp")]
use smoltcp::phy::{Device, DeviceCapabilities, RxToken, TxToken};

const ETHERNET_FRAME_HEADER_SIZE: usize = 18;

const MTU: usize = crate::CONFIG.mtu;

#[cfg(feature = "utils")]
pub mod utils;

#[cfg(coex)]
use include::{coex_adapter_funcs_t, coex_pre_init, esp_coex_adapter_register};

use crate::{
    binary::{
        c_types,
        include::{
            self, __BindgenBitfieldUnit, esp_err_t, esp_interface_t_ESP_IF_WIFI_AP,
            esp_interface_t_ESP_IF_WIFI_STA, esp_supplicant_init, esp_wifi_connect,
            esp_wifi_disconnect, esp_wifi_get_mode, esp_wifi_init_internal,
            esp_wifi_internal_free_rx_buffer, esp_wifi_internal_reg_rxcb, esp_wifi_internal_tx,
            esp_wifi_scan_start, esp_wifi_set_config, esp_wifi_set_country, esp_wifi_set_mode,
            esp_wifi_set_protocol, esp_wifi_set_ps, esp_wifi_set_tx_done_cb, esp_wifi_start,
            esp_wifi_stop, g_wifi_default_wpa_crypto_funcs, wifi_active_scan_time_t,
            wifi_ap_config_t, wifi_auth_mode_t, wifi_cipher_type_t_WIFI_CIPHER_TYPE_CCMP,
            wifi_config_t, wifi_country_policy_t_WIFI_COUNTRY_POLICY_MANUAL, wifi_country_t,
            wifi_init_config_t, wifi_interface_t, wifi_interface_t_WIFI_IF_AP,
            wifi_interface_t_WIFI_IF_STA, wifi_mode_t, wifi_mode_t_WIFI_MODE_AP,
            wifi_mode_t_WIFI_MODE_APSTA, wifi_mode_t_WIFI_MODE_NULL, wifi_mode_t_WIFI_MODE_STA,
            wifi_osi_funcs_t, wifi_pmf_config_t, wifi_scan_config_t, wifi_scan_threshold_t,
            wifi_scan_time_t, wifi_scan_type_t_WIFI_SCAN_TYPE_ACTIVE,
            wifi_scan_type_t_WIFI_SCAN_TYPE_PASSIVE, wifi_sort_method_t_WIFI_CONNECT_AP_BY_SIGNAL,
            wifi_sta_config_t, wpa_crypto_funcs_t, ESP_WIFI_OS_ADAPTER_MAGIC,
            ESP_WIFI_OS_ADAPTER_VERSION, WIFI_INIT_CONFIG_MAGIC,
        },
    },
    compat::queue::SimpleQueue,
};

trait AuthMethodExt {
    fn to_raw(&self) -> wifi_auth_mode_t;
    fn from_raw(raw: wifi_auth_mode_t) -> Self;
}

impl AuthMethodExt for AuthMethod {
    fn to_raw(&self) -> wifi_auth_mode_t {
        match self {
            AuthMethod::None => include::wifi_auth_mode_t_WIFI_AUTH_OPEN,
            AuthMethod::WEP => include::wifi_auth_mode_t_WIFI_AUTH_WEP,
            AuthMethod::WPA => include::wifi_auth_mode_t_WIFI_AUTH_WPA_PSK,
            AuthMethod::WPA2Personal => include::wifi_auth_mode_t_WIFI_AUTH_WPA2_PSK,
            AuthMethod::WPAWPA2Personal => include::wifi_auth_mode_t_WIFI_AUTH_WPA_WPA2_PSK,
            AuthMethod::WPA2Enterprise => include::wifi_auth_mode_t_WIFI_AUTH_WPA2_ENTERPRISE,
            AuthMethod::WPA3Personal => include::wifi_auth_mode_t_WIFI_AUTH_WPA3_PSK,
            AuthMethod::WPA2WPA3Personal => include::wifi_auth_mode_t_WIFI_AUTH_WPA2_WPA3_PSK,
            AuthMethod::WAPIPersonal => include::wifi_auth_mode_t_WIFI_AUTH_WAPI_PSK,
        }
    }

    fn from_raw(raw: wifi_auth_mode_t) -> Self {
        match raw {
            include::wifi_auth_mode_t_WIFI_AUTH_OPEN => AuthMethod::None,
            include::wifi_auth_mode_t_WIFI_AUTH_WEP => AuthMethod::WEP,
            include::wifi_auth_mode_t_WIFI_AUTH_WPA_PSK => AuthMethod::WPA,
            include::wifi_auth_mode_t_WIFI_AUTH_WPA2_PSK => AuthMethod::WPA2Personal,
            include::wifi_auth_mode_t_WIFI_AUTH_WPA_WPA2_PSK => AuthMethod::WPAWPA2Personal,
            include::wifi_auth_mode_t_WIFI_AUTH_WPA2_ENTERPRISE => AuthMethod::WPA2Enterprise,
            include::wifi_auth_mode_t_WIFI_AUTH_WPA3_PSK => AuthMethod::WPA3Personal,
            include::wifi_auth_mode_t_WIFI_AUTH_WPA2_WPA3_PSK => AuthMethod::WPA2WPA3Personal,
            include::wifi_auth_mode_t_WIFI_AUTH_WAPI_PSK => AuthMethod::WAPIPersonal,
            _ => unreachable!(),
        }
    }
}

/// Wifi Mode (Sta and/or Ap)
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum WifiMode {
    Sta,
    Ap,
    ApSta,
}

impl WifiMode {
    pub(crate) fn current() -> Result<Self, WifiError> {
        let mut mode = wifi_mode_t_WIFI_MODE_NULL;
        esp_wifi_result!(unsafe { esp_wifi_get_mode(&mut mode) })?;

        Self::try_from(mode)
    }

    /// Returns true if this mode works as a client
    pub fn is_sta(&self) -> bool {
        match self {
            Self::Sta | Self::ApSta => true,
            Self::Ap => false,
        }
    }

    /// Returns true if this mode works as an access point
    pub fn is_ap(&self) -> bool {
        match self {
            Self::Sta => false,
            Self::Ap | Self::ApSta => true,
        }
    }
}

impl TryFrom<&Configuration> for WifiMode {
    type Error = WifiError;

    fn try_from(config: &Configuration) -> Result<Self, Self::Error> {
        let mode = match config {
            Configuration::None => return Err(WifiError::UnknownWifiMode),
            Configuration::AccessPoint(_) => Self::Ap,
            Configuration::Client(_) => Self::Sta,
            Configuration::Mixed(_, _) => Self::ApSta,
        };

        Ok(mode)
    }
}

impl TryFrom<wifi_mode_t> for WifiMode {
    type Error = WifiError;

    fn try_from(value: wifi_mode_t) -> Result<Self, Self::Error> {
        #[allow(non_upper_case_globals)]
        match value {
            include::wifi_mode_t_WIFI_MODE_STA => Ok(Self::Sta),
            include::wifi_mode_t_WIFI_MODE_AP => Ok(Self::Ap),
            include::wifi_mode_t_WIFI_MODE_APSTA => Ok(Self::ApSta),
            _ => Err(WifiError::UnknownWifiMode),
        }
    }
}

impl Into<wifi_mode_t> for WifiMode {
    fn into(self) -> wifi_mode_t {
        #[allow(non_upper_case_globals)]
        match self {
            Self::Sta => wifi_mode_t_WIFI_MODE_STA,
            Self::Ap => wifi_mode_t_WIFI_MODE_AP,
            Self::ApSta => wifi_mode_t_WIFI_MODE_APSTA,
        }
    }
}

const DATA_FRAME_SIZE: usize = MTU + ETHERNET_FRAME_HEADER_SIZE;

const RX_QUEUE_SIZE: usize = crate::CONFIG.rx_queue_size;
const TX_QUEUE_SIZE: usize = crate::CONFIG.tx_queue_size;

pub(crate) static DATA_QUEUE_RX_AP: Mutex<
    RefCell<SimpleQueue<EspWifiPacketBuffer, RX_QUEUE_SIZE>>,
> = Mutex::new(RefCell::new(SimpleQueue::new()));

pub(crate) static DATA_QUEUE_RX_STA: Mutex<
    RefCell<SimpleQueue<EspWifiPacketBuffer, RX_QUEUE_SIZE>>,
> = Mutex::new(RefCell::new(SimpleQueue::new()));

/// Common errors
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum WifiError {
    NotInitialized,
    InternalError(InternalWifiError),
    WrongClockConfig,
    Disconnected,
    UnknownWifiMode,
}

/// Events generated by the WiFi driver
#[repr(i32)]
#[derive(Debug, FromPrimitive, EnumSetType)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum WifiEvent {
    WifiReady = 0,
    ScanDone,
    StaStart,
    StaStop,
    StaConnected,
    StaDisconnected,
    StaAuthmodeChange,
    StaWpsErSuccess,
    StaWpsErFailed,
    StaWpsErTimeout,
    StaWpsErPin,
    StaWpsErPbcOverlap,
    ApStart,
    ApStop,
    ApStaconnected,
    ApStadisconnected,
    ApProbereqrecved,
    FtmReport,
    StaBssRssiLow,
    ActionTxStatus,
    RocDone,
    StaBeaconTimeout,
}

/// Error originating from the underlying drivers
#[repr(i32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, FromPrimitive)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum InternalWifiError {
    /// Out of memory
    EspErrNoMem = 0x101,

    /// Invalid argument
    EspErrInvalidArg = 0x102,

    /// WiFi driver was not installed by esp_wifi_init
    EspErrWifiNotInit = 0x3001,

    /// WiFi driver was not started by esp_wifi_start
    EspErrWifiNotStarted = 0x3002,

    /// WiFi driver was not stopped by esp_wifi_stop
    EspErrWifiNotStopped = 0x3003,

    /// WiFi interface error
    EspErrWifiIf = 0x3004,

    /// WiFi mode error
    EspErrWifiMode = 0x3005,

    /// WiFi internal state error
    EspErrWifiState = 0x3006,

    /// WiFi internal control block of station or soft-AP error
    EspErrWifiConn = 0x3007,

    /// WiFi internal NVS module error
    EspErrWifiNvs = 0x3008,

    /// MAC address is invalid
    EspErrWifiMac = 0x3009,

    /// SSID is invalid
    EspErrWifiSsid = 0x300A,

    /// Password is invalid
    EspErrWifiPassword = 0x300B,

    /// Timeout error
    EspErrWifiTimeout = 0x300C,

    /// WiFi is in sleep state(RF closed) and wakeup fail
    EspErrWifiWakeFail = 0x300D,

    /// The caller would block
    EspErrWifiWouldBlock = 0x300E,

    /// Station still in disconnect status
    EspErrWifiNotConnect = 0x300F,

    /// Failed to post the event to WiFi task
    EspErrWifiPost = 0x3012,

    /// Invalid WiFi state when init/deinit is called
    EspErrWifiInitState = 0x3013,

    /// Returned when WiFi is stopping
    EspErrWifiStopState = 0x3014,

    /// The WiFi connection is not associated
    EspErrWifiNotAssoc = 0x3015,

    /// The WiFi TX is disallowed
    EspErrWifiTxDisallow = 0x3016,
}

#[cfg(all(coex, any(esp32, esp32c2, esp32c3, esp32c6, esp32s3)))]
static mut G_COEX_ADAPTER_FUNCS: coex_adapter_funcs_t = coex_adapter_funcs_t {
    _version: include::COEX_ADAPTER_VERSION as i32,
    _task_yield_from_isr: Some(task_yield_from_isr),
    _semphr_create: Some(semphr_create),
    _semphr_delete: Some(semphr_delete),
    _semphr_take_from_isr: Some(semphr_take_from_isr_wrapper),
    _semphr_give_from_isr: Some(semphr_give_from_isr_wrapper),
    _semphr_take: Some(semphr_take),
    _semphr_give: Some(semphr_give),
    _is_in_isr: Some(is_in_isr_wrapper),
    _malloc_internal: Some(malloc),
    _free: Some(free),
    _esp_timer_get_time: Some(esp_timer_get_time),
    _env_is_chip: Some(env_is_chip),
    _magic: include::COEX_ADAPTER_MAGIC as i32,
    _timer_disarm: Some(ets_timer_disarm),
    _timer_done: Some(ets_timer_done),
    _timer_setfn: Some(ets_timer_setfn),
    _timer_arm_us: Some(ets_timer_arm_us),

    #[cfg(esp32)]
    _spin_lock_create: Some(spin_lock_create),
    #[cfg(esp32)]
    _spin_lock_delete: Some(spin_lock_delete),
    #[cfg(esp32)]
    _int_disable: Some(wifi_int_disable),
    #[cfg(esp32)]
    _int_enable: Some(wifi_int_restore),

    #[cfg(esp32c2)]
    _slowclk_cal_get: Some(slowclk_cal_get),
};

#[cfg(coex)]
unsafe extern "C" fn semphr_take_from_isr_wrapper(
    semphr: *mut c_types::c_void,
    hptw: *mut c_types::c_void,
) -> i32 {
    crate::common_adapter::semphr_take_from_isr(semphr as *const (), hptw as *const ())
}

#[cfg(coex)]
unsafe extern "C" fn semphr_give_from_isr_wrapper(
    semphr: *mut c_types::c_void,
    hptw: *mut c_types::c_void,
) -> i32 {
    crate::common_adapter::semphr_give_from_isr(semphr as *const (), hptw as *const ())
}

#[cfg(coex)]
unsafe extern "C" fn is_in_isr_wrapper() -> i32 {
    // like original implementation
    0
}

#[cfg(coex)]
pub(crate) fn coex_initialize() -> i32 {
    debug!("call coex-initialize");
    unsafe {
        let res = esp_coex_adapter_register(core::ptr::addr_of_mut!(G_COEX_ADAPTER_FUNCS).cast());
        if res != 0 {
            error!("Error: esp_coex_adapter_register {}", res);
            return res;
        }
        let res = coex_pre_init();
        if res != 0 {
            error!("Error: coex_pre_init {}", res);
            return res;
        }
        0
    }
}

pub(crate) unsafe extern "C" fn coex_init() -> i32 {
    #[cfg(coex)]
    {
        debug!("coex-init");
        return include::coex_init();
    }

    #[cfg(not(coex))]
    0
}

#[no_mangle]
static g_wifi_osi_funcs: wifi_osi_funcs_t = wifi_osi_funcs_t {
    _version: ESP_WIFI_OS_ADAPTER_VERSION as i32,
    _env_is_chip: Some(env_is_chip),
    _set_intr: Some(set_intr),
    _clear_intr: Some(clear_intr),
    _set_isr: Some(os_adapter_chip_specific::set_isr),
    _ints_on: Some(ints_on),
    _ints_off: Some(ints_off),
    _is_from_isr: Some(is_from_isr),
    _spin_lock_create: Some(spin_lock_create),
    _spin_lock_delete: Some(spin_lock_delete),
    _wifi_int_disable: Some(wifi_int_disable),
    _wifi_int_restore: Some(wifi_int_restore),
    _task_yield_from_isr: Some(task_yield_from_isr),
    _semphr_create: Some(semphr_create),
    _semphr_delete: Some(semphr_delete),
    _semphr_take: Some(semphr_take),
    _semphr_give: Some(semphr_give),
    _wifi_thread_semphr_get: Some(wifi_thread_semphr_get),
    _mutex_create: Some(mutex_create),
    _recursive_mutex_create: Some(recursive_mutex_create),
    _mutex_delete: Some(mutex_delete),
    _mutex_lock: Some(mutex_lock),
    _mutex_unlock: Some(mutex_unlock),
    _queue_create: Some(queue_create),
    _queue_delete: Some(queue_delete),
    _queue_send: Some(queue_send),
    _queue_send_from_isr: Some(queue_send_from_isr),
    _queue_send_to_back: Some(queue_send_to_back),
    _queue_send_to_front: Some(queue_send_to_front),
    _queue_recv: Some(queue_recv),
    _queue_msg_waiting: Some(queue_msg_waiting),
    _event_group_create: Some(event_group_create),
    _event_group_delete: Some(event_group_delete),
    _event_group_set_bits: Some(event_group_set_bits),
    _event_group_clear_bits: Some(event_group_clear_bits),
    _event_group_wait_bits: Some(event_group_wait_bits),
    _task_create_pinned_to_core: Some(task_create_pinned_to_core),
    _task_create: Some(task_create),
    _task_delete: Some(task_delete),
    _task_delay: Some(task_delay),
    _task_ms_to_tick: Some(task_ms_to_tick),
    _task_get_current_task: Some(task_get_current_task),
    _task_get_max_priority: Some(task_get_max_priority),
    _malloc: Some(malloc),
    _free: Some(free),
    _event_post: Some(event_post),
    _get_free_heap_size: Some(get_free_heap_size),
    _rand: Some(rand),
    _dport_access_stall_other_cpu_start_wrap: Some(dport_access_stall_other_cpu_start_wrap),
    _dport_access_stall_other_cpu_end_wrap: Some(dport_access_stall_other_cpu_end_wrap),
    _wifi_apb80m_request: Some(wifi_apb80m_request),
    _wifi_apb80m_release: Some(wifi_apb80m_release),
    _phy_disable: Some(phy_disable),
    _phy_enable: Some(phy_enable),
    _phy_update_country_info: Some(phy_update_country_info),
    _read_mac: Some(read_mac),
    _timer_arm: Some(ets_timer_arm),
    _timer_disarm: Some(ets_timer_disarm),
    _timer_done: Some(ets_timer_done),
    _timer_setfn: Some(ets_timer_setfn),
    _timer_arm_us: Some(ets_timer_arm_us),
    _wifi_reset_mac: Some(wifi_reset_mac),
    _wifi_clock_enable: Some(wifi_clock_enable),
    _wifi_clock_disable: Some(wifi_clock_disable),
    _wifi_rtc_enable_iso: Some(wifi_rtc_enable_iso),
    _wifi_rtc_disable_iso: Some(wifi_rtc_disable_iso),
    _esp_timer_get_time: Some(esp_timer_get_time),
    _nvs_set_i8: Some(nvs_set_i8),
    _nvs_get_i8: Some(nvs_get_i8),
    _nvs_set_u8: Some(nvs_set_u8),
    _nvs_get_u8: Some(nvs_get_u8),
    _nvs_set_u16: Some(nvs_set_u16),
    _nvs_get_u16: Some(nvs_get_u16),
    _nvs_open: Some(nvs_open),
    _nvs_close: Some(nvs_close),
    _nvs_commit: Some(nvs_commit),
    _nvs_set_blob: Some(nvs_set_blob),
    _nvs_get_blob: Some(nvs_get_blob),
    _nvs_erase_key: Some(nvs_erase_key),
    _get_random: Some(get_random),
    _get_time: Some(get_time),
    _random: Some(random),
    _log_write: Some(log_write),
    _log_writev: Some(log_writev),
    _log_timestamp: Some(log_timestamp),
    _malloc_internal: Some(malloc_internal),
    _realloc_internal: Some(realloc_internal),
    _calloc_internal: Some(calloc_internal),
    _zalloc_internal: Some(zalloc_internal),
    _wifi_malloc: Some(wifi_malloc),
    _wifi_realloc: Some(wifi_realloc),
    _wifi_calloc: Some(wifi_calloc),
    _wifi_zalloc: Some(wifi_zalloc),
    _wifi_create_queue: Some(wifi_create_queue),
    _wifi_delete_queue: Some(wifi_delete_queue),
    _coex_init: Some(coex_init),
    _coex_deinit: Some(coex_deinit),
    _coex_enable: Some(coex_enable),
    _coex_disable: Some(coex_disable),
    _coex_status_get: Some(coex_status_get),
    _coex_condition_set: None,
    _coex_wifi_request: Some(coex_wifi_request),
    _coex_wifi_release: Some(coex_wifi_release),
    _coex_wifi_channel_set: Some(coex_wifi_channel_set),
    _coex_event_duration_get: Some(coex_event_duration_get),
    _coex_pti_get: Some(coex_pti_get),
    _coex_schm_status_bit_clear: Some(coex_schm_status_bit_clear),
    _coex_schm_status_bit_set: Some(coex_schm_status_bit_set),
    _coex_schm_interval_set: Some(coex_schm_interval_set),
    _coex_schm_interval_get: Some(coex_schm_interval_get),
    _coex_schm_curr_period_get: Some(coex_schm_curr_period_get),
    _coex_schm_curr_phase_get: Some(coex_schm_curr_phase_get),
    #[cfg(any(esp32c3, esp32c2, esp32c6, esp32h2, esp32s3, esp32s2))]
    _slowclk_cal_get: Some(slowclk_cal_get),
    #[cfg(any(esp32, esp32s2))]
    _phy_common_clock_disable: Some(os_adapter_chip_specific::phy_common_clock_disable),
    #[cfg(any(esp32, esp32s2))]
    _phy_common_clock_enable: Some(os_adapter_chip_specific::phy_common_clock_enable),
    _coex_register_start_cb: Some(coex_register_start_cb),

    #[cfg(any(esp32c6))]
    _regdma_link_set_write_wait_content: Some(
        os_adapter_chip_specific::regdma_link_set_write_wait_content_dummy,
    ),
    #[cfg(any(esp32c6))]
    _sleep_retention_find_link_by_id: Some(
        os_adapter_chip_specific::sleep_retention_find_link_by_id_dummy,
    ),
    #[cfg(any(esp32c6))]
    _sleep_retention_entries_create: Some(
        os_adapter_chip_specific::sleep_retention_entries_create_dummy,
    ),
    #[cfg(any(esp32c6))]
    _sleep_retention_entries_destroy: Some(
        os_adapter_chip_specific::sleep_retention_entries_destroy_dummy,
    ),

    _coex_schm_process_restart: Some(coex_schm_process_restart_wrapper),
    _coex_schm_register_cb: Some(coex_schm_register_cb_wrapper),

    _magic: ESP_WIFI_OS_ADAPTER_MAGIC as i32,
};

const CONFIG_FEATURE_WPA3_SAE_BIT: u64 = 1 << 0;

const WIFI_FEATURE_CAPS: u64 = CONFIG_FEATURE_WPA3_SAE_BIT;

#[no_mangle]
static mut g_wifi_feature_caps: u64 = WIFI_FEATURE_CAPS;

static mut G_CONFIG: wifi_init_config_t = wifi_init_config_t {
    osi_funcs: addr_of!(g_wifi_osi_funcs).cast_mut(),

    // dummy for now - populated in init
    wpa_crypto_funcs: wpa_crypto_funcs_t {
        size: 0,
        version: 1,
        aes_wrap: None,
        aes_unwrap: None,
        hmac_sha256_vector: None,
        sha256_prf: None,
        hmac_md5: None,
        hamc_md5_vector: None,
        hmac_sha1: None,
        hmac_sha1_vector: None,
        sha1_prf: None,
        sha1_vector: None,
        pbkdf2_sha1: None,
        rc4_skip: None,
        md5_vector: None,
        aes_encrypt: None,
        aes_encrypt_init: None,
        aes_encrypt_deinit: None,
        aes_decrypt: None,
        aes_decrypt_init: None,
        aes_decrypt_deinit: None,
        aes_128_encrypt: None,
        aes_128_decrypt: None,
        omac1_aes_128: None,
        ccmp_decrypt: None,
        ccmp_encrypt: None,
        aes_gmac: None,
        sha256_vector: None,
        crc32: None,
    },
    static_rx_buf_num: crate::CONFIG.static_rx_buf_num as i32,
    dynamic_rx_buf_num: crate::CONFIG.dynamic_rx_buf_num as i32,
    tx_buf_type: 1,
    static_tx_buf_num: crate::CONFIG.static_tx_buf_num as i32,
    dynamic_tx_buf_num: crate::CONFIG.dynamic_tx_buf_num as i32,
    rx_mgmt_buf_type: 0 as i32,
    rx_mgmt_buf_num: 0 as i32,
    cache_tx_buf_num: 0,
    csi_enable: 1,
    ampdu_rx_enable: crate::CONFIG.ampdu_rx_enable as i32,
    ampdu_tx_enable: crate::CONFIG.ampdu_tx_enable as i32,
    amsdu_tx_enable: crate::CONFIG.amsdu_tx_enable as i32,
    nvs_enable: 0,
    nano_enable: 0,
    rx_ba_win: crate::CONFIG.rx_ba_win as i32,
    wifi_task_core_id: 0,
    beacon_max_len: 752,
    mgmt_sbuf_num: 32,
    feature_caps: WIFI_FEATURE_CAPS,
    sta_disconnected_pm: false,
    espnow_max_encrypt_num: 7, // 2 for ESP32-C2
    magic: WIFI_INIT_CONFIG_MAGIC as i32,
};

/// Get the STA MAC address
pub fn get_sta_mac(mac: &mut [u8; 6]) {
    unsafe {
        read_mac(mac as *mut u8, 0);
    }
}

/// Get the AP MAC address
pub fn get_ap_mac(mac: &mut [u8; 6]) {
    unsafe {
        read_mac(mac as *mut u8, 1);
    }
}

pub(crate) fn wifi_init() -> Result<(), WifiError> {
    unsafe {
        G_CONFIG.wpa_crypto_funcs = g_wifi_default_wpa_crypto_funcs;
        G_CONFIG.feature_caps = g_wifi_feature_caps;

        #[cfg(coex)]
        esp_wifi_result!(coex_init())?;

        esp_wifi_result!(esp_wifi_init_internal(&G_CONFIG))?;
        esp_wifi_result!(esp_wifi_set_mode(wifi_mode_t_WIFI_MODE_NULL))?;

        esp_wifi_result!(esp_supplicant_init())?;

        esp_wifi_result!(esp_wifi_set_tx_done_cb(Some(esp_wifi_tx_done_cb)))?;

        esp_wifi_result!(esp_wifi_internal_reg_rxcb(
            esp_interface_t_ESP_IF_WIFI_STA,
            Some(recv_cb_sta)
        ))?;

        // until we support APSTA we just register the same callback for AP and STA
        esp_wifi_result!(esp_wifi_internal_reg_rxcb(
            esp_interface_t_ESP_IF_WIFI_AP,
            Some(recv_cb_ap)
        ))?;

        #[cfg(any(esp32, esp32s3))]
        {
            static mut NVS_STRUCT: [u32; 12] = [0; 12];
            chip_specific::g_misc_nvs = addr_of!(NVS_STRUCT) as u32;
        }

        Ok(())
    }
}

unsafe extern "C" fn recv_cb_sta(
    buffer: *mut c_types::c_void,
    len: u16,
    eb: *mut c_types::c_void,
) -> esp_err_t {
    let packet = EspWifiPacketBuffer { buffer, len, eb };
    // We must handle the result outside of the critical section because
    // EspWifiPacketBuffer::drop must not be called in a critical section.
    // Dropping an EspWifiPacketBuffer will call `esp_wifi_internal_free_rx_buffer` which
    // will try to lock an internal mutex. If the mutex is already taken, the function will
    // try to trigger a context switch, which will fail if we are in a critical section.
    match critical_section::with(|cs| DATA_QUEUE_RX_STA.borrow_ref_mut(cs).enqueue(packet)) {
        Ok(_) => {
            #[cfg(feature = "embassy-net")]
            embassy::STA_RECEIVE_WAKER.wake();
            include::ESP_OK as esp_err_t
        }
        Err(_) => {
            debug!("RX QUEUE FULL");
            include::ESP_ERR_NO_MEM as esp_err_t
        }
    }
}

unsafe extern "C" fn recv_cb_ap(
    buffer: *mut c_types::c_void,
    len: u16,
    eb: *mut c_types::c_void,
) -> esp_err_t {
    let packet = EspWifiPacketBuffer { buffer, len, eb };
    // We must handle the result outside of the critical section because
    // EspWifiPacketBuffer::drop must not be called in a critical section.
    // Dropping an EspWifiPacketBuffer will call `esp_wifi_internal_free_rx_buffer` which
    // will try to lock an internal mutex. If the mutex is already taken, the function will
    // try to trigger a context switch, which will fail if we are in a critical section.
    match critical_section::with(|cs| DATA_QUEUE_RX_AP.borrow_ref_mut(cs).enqueue(packet)) {
        Ok(_) => {
            #[cfg(feature = "embassy-net")]
            embassy::AP_RECEIVE_WAKER.wake();
            include::ESP_OK as esp_err_t
        }
        Err(_) => {
            debug!("RX QUEUE FULL");
            include::ESP_ERR_NO_MEM as esp_err_t
        }
    }
}

pub(crate) static WIFI_TX_INFLIGHT: AtomicUsize = AtomicUsize::new(0);

fn decrement_inflight_counter() {
    WIFI_TX_INFLIGHT
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |x| {
            Some(x.saturating_sub(1))
        })
        .unwrap();
}

#[ram]
unsafe extern "C" fn esp_wifi_tx_done_cb(
    _ifidx: u8,
    _data: *mut u8,
    _data_len: *mut u16,
    _tx_status: bool,
) {
    trace!("esp_wifi_tx_done_cb");

    decrement_inflight_counter();

    #[cfg(feature = "embassy-net")]
    embassy::TRANSMIT_WAKER.wake();
}

pub(crate) fn wifi_start() -> Result<(), WifiError> {
    unsafe {
        esp_wifi_result!(esp_wifi_start())?;

        let mode = WifiMode::current()?;

        // This is not an if-else because in AP-STA mode, both are true
        if mode.is_ap() {
            esp_wifi_result!(include::esp_wifi_set_inactive_time(
                wifi_interface_t_WIFI_IF_AP,
                crate::CONFIG.ap_beacon_timeout
            ))?;
        }
        if mode.is_sta() {
            esp_wifi_result!(include::esp_wifi_set_inactive_time(
                wifi_interface_t_WIFI_IF_STA,
                crate::CONFIG.beacon_timeout
            ))?;
        };

        let ps_mode;
        cfg_if::cfg_if! {
            if #[cfg(feature = "ps-min-modem")] {
                ps_mode = include::wifi_ps_type_t_WIFI_PS_MIN_MODEM;
            } else if #[cfg(feature = "ps-max-modem")] {
                ps_mode = include::wifi_ps_type_t_WIFI_PS_MAX_MODEM;
            } else if #[cfg(coex)] {
                ps_mode = include::wifi_ps_type_t_WIFI_PS_MIN_MODEM;
            } else {
                ps_mode = include::wifi_ps_type_t_WIFI_PS_NONE;
            }
        };

        esp_wifi_result!(esp_wifi_set_ps(ps_mode))?;

        let mut cntry_code = [0u8; 3];
        cntry_code[..crate::CONFIG.country_code.len()]
            .copy_from_slice(crate::CONFIG.country_code.as_bytes());
        cntry_code[2] = crate::CONFIG.country_code_operating_class;

        let country = wifi_country_t {
            cc: core::mem::transmute(cntry_code), // [u8] -> [i8] conversion
            schan: 1,
            nchan: 13,
            max_tx_power: 20,
            policy: wifi_country_policy_t_WIFI_COUNTRY_POLICY_MANUAL,
        };
        esp_wifi_result!(esp_wifi_set_country(&country))?;
    }

    Ok(())
}

unsafe extern "C" fn coex_register_start_cb(
    _cb: Option<unsafe extern "C" fn() -> c_types::c_int>,
) -> c_types::c_int {
    #[cfg(coex)]
    return include::coex_register_start_cb(_cb);

    #[cfg(not(coex))]
    0
}

/// Configuration for active or passive scan. For details see the [WIFI Alliance FAQ](https://www.wi-fi.org/knowledge-center/faq/what-are-passive-and-active-scanning).
///
/// # Comparison of active and passive scan
///
///|                                      | **Active** | **Passive** |
///|--------------------------------------|------------|-------------|
///| **Power consumption**                |    High    |     Low     |
///| **Time required (typical behavior)** |     Low    |     High    |
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ScanTypeConfig {
    /// Active scan with min and max scan time per channel. This is the default and recommended if
    /// you are unsure.
    ///
    /// # Procedure
    /// 1. Send probe request on each channel.
    /// 2. Wait for probe response. Wait at least `min` time, but if no response is received, wait up to `max` time.
    /// 3. Switch channel.
    /// 4. Repeat from 1.
    Active {
        /// Minimum scan time per channel. Defaults to 10ms.
        min: Duration,
        /// Maximum scan time per channel. Defaults to 20ms.
        max: Duration,
    },
    /// Passive scan
    ///
    /// # Procedure
    /// 1. Wait for beacon for given duration.
    /// 2. Switch channel.
    /// 3. Repeat from 1.
    ///
    /// # Note
    /// It is recommended to avoid duration longer thean 1500ms, as it may cause a station to
    /// disconnect from the AP.
    Passive(Duration),
}

impl Default for ScanTypeConfig {
    fn default() -> Self {
        Self::Active {
            min: Duration::from_millis(10),
            max: Duration::from_millis(20),
        }
    }
}

impl ScanTypeConfig {
    fn validate(&self) {
        if matches!(self, Self::Passive(dur) if *dur > Duration::from_millis(1500)) {
            warn!("Passive scan duration longer than 1500ms may cause a station to disconnect from the AP");
        }
    }
}

/// Scan configuration
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanConfig<'a> {
    /// SSID to filter for.
    /// If [`None`] is passed, all SSIDs will be returned.
    /// If [`Some`] is passed, only the APs matching the given SSID will be returned.
    pub ssid: Option<&'a str>,
    /// BSSID to filter for.
    /// If [`None`] is passed, all BSSIDs will be returned.
    /// If [`Some`] is passed, only the APs matching the given BSSID will be returned.
    pub bssid: Option<[u8; 6]>,
    /// Channel to filter for.
    /// If [`None`] is passed, all channels will be returned.
    /// If [`Some`] is passed, only the APs on the given channel will be returned.
    pub channel: Option<u8>,
    /// Whether to show hidden networks.
    pub show_hidden: bool,
    /// Scan type, active or passive.
    pub scan_type: ScanTypeConfig,
}

pub(crate) fn wifi_start_scan(
    block: bool,
    ScanConfig {
        ssid,
        mut bssid,
        channel,
        show_hidden,
        scan_type,
    }: ScanConfig<'_>,
) -> i32 {
    scan_type.validate();
    let (scan_time, scan_type) = match scan_type {
        ScanTypeConfig::Active { min, max } => (
            wifi_scan_time_t {
                active: wifi_active_scan_time_t {
                    min: min.as_millis() as u32,
                    max: max.as_millis() as u32,
                },
                passive: 0,
            },
            wifi_scan_type_t_WIFI_SCAN_TYPE_ACTIVE,
        ),
        ScanTypeConfig::Passive(dur) => (
            wifi_scan_time_t {
                active: wifi_active_scan_time_t { min: 0, max: 0 },
                passive: dur.as_millis() as u32,
            },
            wifi_scan_type_t_WIFI_SCAN_TYPE_PASSIVE,
        ),
    };

    let mut ssid_buf = ssid.map(|m| {
        let mut buf = heapless::Vec::<u8, 33>::from_iter(m.bytes());
        unwrap!(buf.push(b'\0').ok());
        buf
    });

    let ssid = ssid_buf
        .as_mut()
        .map(|e| e.as_mut_ptr())
        .unwrap_or_else(core::ptr::null_mut);
    let bssid = bssid
        .as_mut()
        .map(|e| e.as_mut_ptr())
        .unwrap_or_else(core::ptr::null_mut);

    let scan_config = wifi_scan_config_t {
        ssid,
        bssid,
        channel: channel.unwrap_or(0),
        show_hidden,
        scan_type,
        scan_time,
        home_chan_dwell_time: 0,
    };

    unsafe { esp_wifi_scan_start(&scan_config, block) }
}

/// Creates a new [WifiDevice] and [WifiController] in either AP or STA mode with the given
/// configuration.
///
/// This function will panic if the configuration is not
/// [`Configuration::Client`] or [`Configuration::Station`].
///
/// If you want to use AP-STA mode, use `[new_ap_sta]`.
pub fn new_with_config<'d, MODE: WifiDeviceMode>(
    inited: &EspWifiInitialization,
    device: impl Peripheral<P = crate::hal::peripherals::WIFI> + 'd,
    config: MODE::Config,
) -> Result<(WifiDevice<'d, MODE>, WifiController<'d>), WifiError> {
    crate::hal::into_ref!(device);

    Ok((
        WifiDevice::new(unsafe { device.clone_unchecked() }, MODE::new()),
        WifiController::new_with_config(inited, device, MODE::wrap_config(config))?,
    ))
}

/// Creates a new [WifiDevice] and [WifiController] in either AP or STA mode with a default
/// configuration.
///
/// This function will panic if the mode is [`WifiMode::ApSta`].
/// If you want to use AP-STA mode, use `[new_ap_sta]`.
pub fn new_with_mode<'d, MODE: WifiDeviceMode>(
    inited: &EspWifiInitialization,
    device: impl crate::hal::peripheral::Peripheral<P = crate::hal::peripherals::WIFI> + 'd,
    _mode: MODE,
) -> Result<(WifiDevice<'d, MODE>, WifiController<'d>), WifiError> {
    new_with_config(inited, device, <MODE as Sealed>::Config::default())
}

/// Creates a new [WifiDevice] and [WifiController] in AP-STA mode, with a default configuration.
///
/// Returns a tuple of `(AP device, STA device, controller)`.
pub fn new_ap_sta<'d>(
    inited: &EspWifiInitialization,
    device: impl Peripheral<P = crate::hal::peripherals::WIFI> + 'd,
) -> Result<
    (
        WifiDevice<'d, WifiApDevice>,
        WifiDevice<'d, WifiStaDevice>,
        WifiController<'d>,
    ),
    WifiError,
> {
    new_ap_sta_with_config(inited, device, Default::default(), Default::default())
}

/// Creates a new Wifi device and controller in AP-STA mode.
///
/// Returns a tuple of `(AP device, STA device, controller)`.
pub fn new_ap_sta_with_config<'d>(
    inited: &EspWifiInitialization,
    device: impl Peripheral<P = crate::hal::peripherals::WIFI> + 'd,
    sta_config: embedded_svc::wifi::ClientConfiguration,
    ap_config: embedded_svc::wifi::AccessPointConfiguration,
) -> Result<
    (
        WifiDevice<'d, WifiApDevice>,
        WifiDevice<'d, WifiStaDevice>,
        WifiController<'d>,
    ),
    WifiError,
> {
    crate::hal::into_ref!(device);

    Ok((
        WifiDevice::new(unsafe { device.clone_unchecked() }, WifiApDevice),
        WifiDevice::new(unsafe { device.clone_unchecked() }, WifiStaDevice),
        WifiController::new_with_config(
            inited,
            device,
            Configuration::Mixed(sta_config, ap_config),
        )?,
    ))
}

mod sealed {
    use super::*;

    #[derive(Debug)]
    #[cfg_attr(feature = "defmt", derive(defmt::Format))]
    /// Take care not to drop this while in a critical section.
    ///
    /// Dropping an EspWifiPacketBuffer will call `esp_wifi_internal_free_rx_buffer` which
    /// will try to lock an internal mutex. If the mutex is already taken, the function will
    /// try to trigger a context switch, which will fail if we are in a critical section.
    pub struct EspWifiPacketBuffer {
        pub(crate) buffer: *mut c_types::c_void,
        pub(crate) len: u16,
        pub(crate) eb: *mut c_types::c_void,
    }

    unsafe impl Send for EspWifiPacketBuffer {}

    impl Drop for EspWifiPacketBuffer {
        fn drop(&mut self) {
            trace!("Dropping EspWifiPacketBuffer, freeing memory");
            unsafe { esp_wifi_internal_free_rx_buffer(self.eb) };
        }
    }

    impl EspWifiPacketBuffer {
        pub fn as_slice_mut(&mut self) -> &mut [u8] {
            unsafe { core::slice::from_raw_parts_mut(self.buffer as *mut u8, self.len as usize) }
        }
    }

    pub trait Sealed: Copy + Sized {
        type Config: Default;

        fn new() -> Self;

        fn wrap_config(config: Self::Config) -> Configuration;

        fn data_queue_rx(
            self,
            cs: CriticalSection,
        ) -> RefMut<'_, SimpleQueue<EspWifiPacketBuffer, RX_QUEUE_SIZE>>;

        fn can_send(self) -> bool {
            WIFI_TX_INFLIGHT.load(Ordering::SeqCst) < TX_QUEUE_SIZE
        }

        fn increase_in_flight_counter(self) {
            WIFI_TX_INFLIGHT.fetch_add(1, Ordering::SeqCst);
        }

        fn tx_token(self) -> Option<WifiTxToken<Self>> {
            if self.can_send() {
                Some(WifiTxToken { mode: self })
            } else {
                warn!("no Tx token available");
                None
            }
        }

        fn rx_token(self) -> Option<(WifiRxToken<Self>, WifiTxToken<Self>)> {
            let is_empty = critical_section::with(|cs| self.data_queue_rx(cs).is_empty());

            if !is_empty {
                self.tx_token().map(|tx| (WifiRxToken { mode: self }, tx))
            } else {
                trace!("no Rx token available");
                None
            }
        }

        fn interface(self) -> wifi_interface_t;

        #[cfg(feature = "embassy-net")]
        fn register_transmit_waker(self, cx: &mut core::task::Context) {
            embassy::TRANSMIT_WAKER.register(cx.waker())
        }

        #[cfg(feature = "embassy-net")]
        fn register_receive_waker(self, cx: &mut core::task::Context);

        #[cfg(feature = "embassy-net")]
        fn register_link_state_waker(self, cx: &mut core::task::Context);

        #[cfg(feature = "embassy-net")]
        fn link_state(self) -> embassy_net_driver::LinkState;
    }

    impl Sealed for WifiStaDevice {
        type Config = ClientConfiguration;

        fn new() -> Self {
            Self
        }

        fn wrap_config(config: ClientConfiguration) -> Configuration {
            Configuration::Client(config)
        }

        fn data_queue_rx(
            self,
            cs: CriticalSection,
        ) -> RefMut<'_, SimpleQueue<EspWifiPacketBuffer, RX_QUEUE_SIZE>> {
            DATA_QUEUE_RX_STA.borrow_ref_mut(cs)
        }

        fn interface(self) -> wifi_interface_t {
            wifi_interface_t_WIFI_IF_STA
        }

        #[cfg(feature = "embassy-net")]
        fn register_receive_waker(self, cx: &mut core::task::Context) {
            embassy::STA_RECEIVE_WAKER.register(cx.waker());
        }

        #[cfg(feature = "embassy-net")]
        fn register_link_state_waker(self, cx: &mut core::task::Context) {
            embassy::STA_LINK_STATE_WAKER.register(cx.waker());
        }

        #[cfg(feature = "embassy-net")]
        fn link_state(self) -> embassy_net_driver::LinkState {
            if matches!(get_sta_state(), WifiState::StaConnected) {
                embassy_net_driver::LinkState::Up
            } else {
                embassy_net_driver::LinkState::Down
            }
        }
    }

    impl Sealed for WifiApDevice {
        type Config = AccessPointConfiguration;

        fn new() -> Self {
            Self
        }

        fn wrap_config(config: AccessPointConfiguration) -> Configuration {
            Configuration::AccessPoint(config)
        }

        fn data_queue_rx(
            self,
            cs: CriticalSection,
        ) -> RefMut<'_, SimpleQueue<EspWifiPacketBuffer, RX_QUEUE_SIZE>> {
            DATA_QUEUE_RX_AP.borrow_ref_mut(cs)
        }

        fn interface(self) -> wifi_interface_t {
            wifi_interface_t_WIFI_IF_AP
        }

        #[cfg(feature = "embassy-net")]
        fn register_receive_waker(self, cx: &mut core::task::Context) {
            embassy::AP_RECEIVE_WAKER.register(cx.waker());
        }

        #[cfg(feature = "embassy-net")]
        fn register_link_state_waker(self, cx: &mut core::task::Context) {
            embassy::AP_LINK_STATE_WAKER.register(cx.waker());
        }

        #[cfg(feature = "embassy-net")]
        fn link_state(self) -> embassy_net_driver::LinkState {
            if matches!(get_ap_state(), WifiState::ApStarted) {
                embassy_net_driver::LinkState::Up
            } else {
                embassy_net_driver::LinkState::Down
            }
        }
    }
}

use sealed::*;

pub trait WifiDeviceMode: Sealed {
    fn mode(self) -> WifiMode;

    fn mac_address(self) -> [u8; 6];
}

#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct WifiStaDevice;

impl WifiDeviceMode for WifiStaDevice {
    fn mode(self) -> WifiMode {
        WifiMode::Sta
    }

    fn mac_address(self) -> [u8; 6] {
        let mut mac = [0; 6];
        get_sta_mac(&mut mac);
        mac
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct WifiApDevice;

impl WifiDeviceMode for WifiApDevice {
    fn mode(self) -> WifiMode {
        WifiMode::Ap
    }

    fn mac_address(self) -> [u8; 6] {
        let mut mac = [0; 6];
        get_ap_mac(&mut mac);
        mac
    }
}

/// A wifi device implementing smoltcp's Device trait.
pub struct WifiDevice<'d, MODE: WifiDeviceMode> {
    _device: PeripheralRef<'d, crate::hal::peripherals::WIFI>,
    mode: MODE,
}

impl<'d, MODE: WifiDeviceMode> WifiDevice<'d, MODE> {
    pub(crate) fn new(
        _device: PeripheralRef<'d, crate::hal::peripherals::WIFI>,
        mode: MODE,
    ) -> Self {
        Self { _device, mode }
    }

    pub fn mac_address(&self) -> [u8; 6] {
        self.mode.mac_address()
    }

    #[cfg(not(feature = "smoltcp"))]
    pub fn receive(&mut self) -> Option<(WifiRxToken<MODE>, WifiTxToken<MODE>)> {
        self.mode.rx_token()
    }

    #[cfg(not(feature = "smoltcp"))]
    pub fn transmit(&mut self) -> Option<WifiTxToken<MODE>> {
        self.mode.tx_token()
    }
}

fn convert_ap_info(record: &include::wifi_ap_record_t) -> AccessPointInfo {
    let str_len = record
        .ssid
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(record.ssid.len());
    let ssid_ref = unsafe { core::str::from_utf8_unchecked(&record.ssid[..str_len]) };

    let mut ssid = heapless::String::<32>::new();
    unwrap!(ssid.push_str(ssid_ref));

    AccessPointInfo {
        ssid,
        bssid: record.bssid,
        channel: record.primary,
        secondary_channel: match record.second {
            include::wifi_second_chan_t_WIFI_SECOND_CHAN_NONE => SecondaryChannel::None,
            include::wifi_second_chan_t_WIFI_SECOND_CHAN_ABOVE => SecondaryChannel::Above,
            include::wifi_second_chan_t_WIFI_SECOND_CHAN_BELOW => SecondaryChannel::Below,
            _ => panic!(),
        },
        signal_strength: record.rssi,
        protocols: EnumSet::empty(), // TODO
        auth_method: Some(AuthMethod::from_raw(record.authmode)),
    }
}

/// A wifi controller implementing embedded_svc::Wifi traits
pub struct WifiController<'d> {
    _device: PeripheralRef<'d, crate::hal::peripherals::WIFI>,
    config: Configuration,
}

impl<'d> WifiController<'d> {
    pub(crate) fn new_with_config(
        inited: &EspWifiInitialization,
        _device: PeripheralRef<'d, crate::hal::peripherals::WIFI>,
        config: Configuration,
    ) -> Result<Self, WifiError> {
        if !inited.is_wifi() {
            return Err(WifiError::NotInitialized);
        }

        // We set up the controller with the default config because we need to call
        // `set_configuration` to apply the actual configuration, and it will update the stored
        // configuration anyway.
        let mut this = Self {
            _device,
            config: Default::default(),
        };

        let mode = WifiMode::try_from(&config)?;
        esp_wifi_result!(unsafe { esp_wifi_set_mode(mode.into()) })?;
        debug!("Wifi mode {:?} set", mode);

        this.set_configuration(&config)?;
        Ok(this)
    }

    /// Set the wifi mode.
    ///
    /// This will set the wifi protocol to the desired protocol, the default for this is:
    /// `WIFI_PROTOCOL_11B|WIFI_PROTOCOL_11G|WIFI_PROTOCOL_11N`
    ///
    /// # Arguments:
    ///
    /// * `protocol` - The desired protocol
    ///
    /// # Example:
    ///
    /// ```
    /// use embedded_svc::wifi::Protocol;
    /// use esp_wifi::wifi::WifiController;
    /// let mut wifi = WifiController::new();
    /// wifi.set_mode(Protocol::P802D11BGNLR);
    /// ```
    pub fn set_mode(&mut self, protocol: Protocol) -> Result<(), WifiError> {
        let mut mode = wifi_mode_t_WIFI_MODE_NULL;
        esp_wifi_result!(unsafe { esp_wifi_get_mode(&mut mode) })?;
        esp_wifi_result!(unsafe { esp_wifi_set_protocol(mode, protocol as u8) })?;
        Ok(())
    }

    pub fn is_sta_enabled(&self) -> Result<bool, WifiError> {
        WifiMode::try_from(&self.config).map(|m| m.is_sta())
    }

    pub fn is_ap_enabled(&self) -> Result<bool, WifiError> {
        WifiMode::try_from(&self.config).map(|m| m.is_ap())
    }

    /// A blocking wifi network scan with caller-provided scanning options.
    pub fn scan_with_config_sync<const N: usize>(
        &mut self,
        config: ScanConfig<'_>,
    ) -> Result<(heapless::Vec<AccessPointInfo, N>, usize), WifiError> {
        esp_wifi_result!(crate::wifi::wifi_start_scan(true, config))?;

        let count = self.scan_result_count()?;
        let result = self.scan_results()?;

        Ok((result, count))
    }

    fn scan_result_count(&mut self) -> Result<usize, WifiError> {
        let mut bss_total: u16 = 0;

        // Prevents memory leak on error
        let guard = FreeApListOnDrop;

        unsafe { esp_wifi_result!(include::esp_wifi_scan_get_ap_num(&mut bss_total))? };

        guard.defuse();

        Ok(bss_total as usize)
    }

    fn scan_results<const N: usize>(
        &mut self,
    ) -> Result<heapless::Vec<AccessPointInfo, N>, WifiError> {
        let mut scanned = heapless::Vec::<AccessPointInfo, N>::new();
        let mut bss_total: u16 = N as u16;

        let mut records: [MaybeUninit<include::wifi_ap_record_t>; N] = [MaybeUninit::uninit(); N];

        // Prevents memory leak on error
        let guard = FreeApListOnDrop;

        unsafe {
            esp_wifi_result!(include::esp_wifi_scan_get_ap_records(
                &mut bss_total,
                records[0].as_mut_ptr(),
            ))?
        };

        guard.defuse();

        for i in 0..bss_total {
            let record = unsafe { MaybeUninit::assume_init_ref(&records[i as usize]) };
            let ap_info = convert_ap_info(record);

            scanned.push(ap_info).ok();
        }

        Ok(scanned)
    }
}

// see https://docs.rs/smoltcp/0.7.1/smoltcp/phy/index.html
#[cfg(feature = "smoltcp")]
impl<MODE: WifiDeviceMode> Device for WifiDevice<'_, MODE> {
    type RxToken<'a> = WifiRxToken<MODE> where Self: 'a;
    type TxToken<'a> = WifiTxToken<MODE> where Self: 'a;

    fn receive(
        &mut self,
        _instant: smoltcp::time::Instant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        self.mode.rx_token()
    }

    fn transmit(&mut self, _instant: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        self.mode.tx_token()
    }

    fn capabilities(&self) -> smoltcp::phy::DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = MTU;
        caps.max_burst_size = if crate::CONFIG.max_burst_size == 0 {
            None
        } else {
            Some(crate::CONFIG.max_burst_size)
        };
        caps
    }
}

#[doc(hidden)]
#[derive(Debug)]
pub struct WifiRxToken<MODE: Sealed> {
    mode: MODE,
}

impl<MODE: Sealed> WifiRxToken<MODE> {
    pub fn consume_token<R, F>(self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut data = critical_section::with(|cs| {
            let mut queue = self.mode.data_queue_rx(cs);

            unwrap!(
                queue.dequeue(),
                "unreachable: transmit()/receive() ensures there is a packet to process"
            )
        });

        // We handle the received data outside of the critical section because
        // EspWifiPacketBuffer::drop must not be called in a critical section.
        // Dropping an EspWifiPacketBuffer will call `esp_wifi_internal_free_rx_buffer` which
        // will try to lock an internal mutex. If the mutex is already taken, the function will
        // try to trigger a context switch, which will fail if we are in a critical section.
        let buffer = data.as_slice_mut();
        dump_packet_info(&buffer);

        f(buffer)
    }
}

#[cfg(feature = "smoltcp")]
impl<MODE: Sealed> RxToken for WifiRxToken<MODE> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        self.consume_token(f)
    }
}

#[doc(hidden)]
#[derive(Debug)]
pub struct WifiTxToken<MODE: Sealed> {
    mode: MODE,
}

impl<MODE: Sealed> WifiTxToken<MODE> {
    pub fn consume_token<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        self.mode.increase_in_flight_counter();

        // (safety): creation of multiple WiFi devices with the same mode is impossible in safe Rust,
        // therefore only smoltcp _or_ embassy-net can be used at one time
        static mut BUFFER: [u8; DATA_FRAME_SIZE] = [0u8; DATA_FRAME_SIZE];

        let buffer = unsafe { &mut BUFFER[..len] };

        let res = f(buffer);

        esp_wifi_send_data(self.mode.interface(), buffer);

        res
    }
}

#[cfg(feature = "smoltcp")]
impl<MODE: Sealed> TxToken for WifiTxToken<MODE> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        self.consume_token(len, f)
    }
}

// FIXME data here has to be &mut because of `esp_wifi_internal_tx` signature, requiring a *mut ptr to the buffer
// Casting const to mut is instant UB, even though in reality `esp_wifi_internal_tx` copies the buffer into its own memory and
// does not modify
pub(crate) fn esp_wifi_send_data(interface: wifi_interface_t, data: &mut [u8]) {
    trace!("sending... {} bytes", data.len());
    dump_packet_info(data);

    let len = data.len() as u16;
    let ptr = data.as_mut_ptr().cast();

    let res = unsafe { esp_wifi_internal_tx(interface, ptr, len) };

    if res != 0 {
        warn!("esp_wifi_internal_tx {}", res);
        decrement_inflight_counter();
    } else {
        trace!("esp_wifi_internal_tx ok");
    }
}

fn apply_ap_config(config: &AccessPointConfiguration) -> Result<(), WifiError> {
    let mut cfg = wifi_config_t {
        ap: wifi_ap_config_t {
            ssid: [0; 32],
            password: [0; 64],
            ssid_len: 0,
            channel: config.channel,
            authmode: config.auth_method.to_raw(),
            ssid_hidden: if config.ssid_hidden { 1 } else { 0 },
            max_connection: config.max_connections as u8,
            beacon_interval: 100,
            pairwise_cipher: wifi_cipher_type_t_WIFI_CIPHER_TYPE_CCMP,
            ftm_responder: false,
            pmf_cfg: wifi_pmf_config_t {
                capable: true,
                required: false,
            },
            sae_pwe_h2e: 0,
        },
    };

    unsafe {
        cfg.ap.ssid[0..(config.ssid.len())].copy_from_slice(config.ssid.as_bytes());
        cfg.ap.ssid_len = config.ssid.len() as u8;
        cfg.ap.password[0..(config.password.len())].copy_from_slice(config.password.as_bytes());

        esp_wifi_result!(esp_wifi_set_config(wifi_interface_t_WIFI_IF_AP, &mut cfg))
    }
}

fn apply_sta_config(config: &ClientConfiguration) -> Result<(), WifiError> {
    let mut cfg = wifi_config_t {
        sta: wifi_sta_config_t {
            ssid: [0; 32],
            password: [0; 64],
            scan_method: crate::CONFIG.scan_method,
            bssid_set: config.bssid.is_some(),
            bssid: match config.bssid {
                Some(bssid_ref) => bssid_ref,
                None => [0; 6],
            },
            channel: config.channel.unwrap_or(0),
            listen_interval: crate::CONFIG.listen_interval,
            sort_method: wifi_sort_method_t_WIFI_CONNECT_AP_BY_SIGNAL,
            threshold: wifi_scan_threshold_t {
                rssi: -99,
                authmode: config.auth_method.to_raw(),
            },
            pmf_cfg: wifi_pmf_config_t {
                capable: true,
                required: false,
            },
            sae_pwe_h2e: 3,
            _bitfield_align_1: [0; 0],
            _bitfield_1: __BindgenBitfieldUnit::new([0; 4]),
            failure_retry_cnt: crate::CONFIG.failure_retry_cnt,
            _bitfield_align_2: [0; 0],
            _bitfield_2: __BindgenBitfieldUnit::new([0; 4]),
            sae_pk_mode: 0, // ??
            sae_h2e_identifier: [0; 32],
        },
    };

    unsafe {
        cfg.sta.ssid[0..(config.ssid.len())].copy_from_slice(config.ssid.as_bytes());
        cfg.sta.password[0..(config.password.len())].copy_from_slice(config.password.as_bytes());

        esp_wifi_result!(esp_wifi_set_config(wifi_interface_t_WIFI_IF_STA, &mut cfg))
    }
}

impl Wifi for WifiController<'_> {
    type Error = WifiError;

    /// This currently only supports the `Client` and `AccessPoint` capability.
    fn get_capabilities(&self) -> Result<EnumSet<embedded_svc::wifi::Capability>, Self::Error> {
        use embedded_svc::wifi::Capability;

        let caps = match self.config {
            Configuration::None => unreachable!(),
            Configuration::Client(_) => enumset::enum_set! { Capability::Client },
            Configuration::AccessPoint(_) => enumset::enum_set! { Capability::AccessPoint },
            Configuration::Mixed(_, _) => {
                Capability::Client | Capability::AccessPoint | Capability::Mixed
            }
        };

        Ok(caps)
    }

    /// A blocking wifi network scan with default scanning options.
    fn scan_n<const N: usize>(
        &mut self,
    ) -> Result<(heapless::Vec<AccessPointInfo, N>, usize), Self::Error> {
        self.scan_with_config_sync(Default::default())
    }

    /// Get the currently used configuration.
    fn get_configuration(&self) -> Result<Configuration, Self::Error> {
        Ok(self.config.clone())
    }

    /// Set the configuration, you need to use Wifi::connect() for connecting to an AP
    /// Trying anything but `Configuration::Client` or `Configuration::AccessPoint` will result in a panic!
    fn set_configuration(&mut self, conf: &Configuration) -> Result<(), Self::Error> {
        match self.config {
            Configuration::None => self.config = conf.clone(), // initial config
            Configuration::Client(ref mut client) => {
                if let Configuration::Client(conf) = conf {
                    *client = conf.clone();
                } else {
                    return Err(WifiError::InternalError(
                        InternalWifiError::EspErrInvalidArg,
                    ));
                }
            }
            Configuration::AccessPoint(ref mut ap) => {
                if let Configuration::AccessPoint(conf) = conf {
                    *ap = conf.clone();
                } else {
                    return Err(WifiError::InternalError(
                        InternalWifiError::EspErrInvalidArg,
                    ));
                }
            }
            Configuration::Mixed(ref mut client, ref mut ap) => match conf {
                Configuration::None => {
                    return Err(WifiError::InternalError(
                        InternalWifiError::EspErrInvalidArg,
                    ));
                }
                Configuration::Mixed(_, _) => self.config = conf.clone(),
                Configuration::Client(conf) => *client = conf.clone(),
                Configuration::AccessPoint(conf) => *ap = conf.clone(),
            },
        }

        match conf {
            Configuration::None => {
                return Err(WifiError::InternalError(
                    InternalWifiError::EspErrInvalidArg,
                ));
            }
            Configuration::Client(config) => apply_sta_config(config)?,
            Configuration::AccessPoint(config) => apply_ap_config(config)?,
            Configuration::Mixed(sta_config, ap_config) => {
                apply_ap_config(ap_config)?;
                apply_sta_config(sta_config)?;
            }
        };

        Ok(())
    }

    fn start(&mut self) -> Result<(), Self::Error> {
        crate::wifi::wifi_start()
    }

    fn stop(&mut self) -> Result<(), Self::Error> {
        esp_wifi_result!(unsafe { esp_wifi_stop() })
    }

    fn connect(&mut self) -> Result<(), Self::Error> {
        esp_wifi_result!(unsafe { esp_wifi_connect() })
    }

    fn disconnect(&mut self) -> Result<(), Self::Error> {
        esp_wifi_result!(unsafe { esp_wifi_disconnect() })
    }

    fn is_started(&self) -> Result<bool, Self::Error> {
        if matches!(
            crate::wifi::get_sta_state(),
            WifiState::StaStarted | WifiState::StaConnected | WifiState::StaDisconnected
        ) {
            return Ok(true);
        }
        if matches!(crate::wifi::get_ap_state(), WifiState::ApStarted) {
            return Ok(true);
        }
        Ok(false)
    }

    fn is_connected(&self) -> Result<bool, Self::Error> {
        match crate::wifi::get_sta_state() {
            crate::wifi::WifiState::StaConnected => Ok(true),
            crate::wifi::WifiState::StaDisconnected => Err(WifiError::Disconnected),
            //FIXME: Should any other enum value trigger an error instead of returning false?
            _ => Ok(false),
        }
    }
}

fn dump_packet_info(_buffer: &[u8]) {
    #[cfg(feature = "dump-packets")]
    {
        info!("@WIFIFRAME {:?}", _buffer);
    }
}

#[doc(hidden)]
#[macro_export]
macro_rules! esp_wifi_result {
    ($value:expr) => {{
        let result = $value;
        if result != include::ESP_OK as i32 {
            warn!("{} returned an error: {}", stringify!($value), result);
            Err(WifiError::InternalError(unwrap!(FromPrimitive::from_i32(
                result
            ))))
        } else {
            Ok::<(), WifiError>(())
        }
    }};
}

#[cfg(feature = "embassy-net")]
pub(crate) mod embassy {
    use super::*;
    use embassy_net_driver::{Capabilities, Driver, HardwareAddress, RxToken, TxToken};
    use embassy_sync::waitqueue::AtomicWaker;

    // We can get away with a single tx waker because the transmit queue is shared
    // between interfaces.
    pub(crate) static TRANSMIT_WAKER: AtomicWaker = AtomicWaker::new();

    pub(crate) static AP_RECEIVE_WAKER: AtomicWaker = AtomicWaker::new();
    pub(crate) static AP_LINK_STATE_WAKER: AtomicWaker = AtomicWaker::new();

    pub(crate) static STA_RECEIVE_WAKER: AtomicWaker = AtomicWaker::new();
    pub(crate) static STA_LINK_STATE_WAKER: AtomicWaker = AtomicWaker::new();

    impl<MODE: WifiDeviceMode> RxToken for WifiRxToken<MODE> {
        fn consume<R, F>(self, f: F) -> R
        where
            F: FnOnce(&mut [u8]) -> R,
        {
            self.consume_token(f)
        }
    }

    impl<MODE: WifiDeviceMode> TxToken for WifiTxToken<MODE> {
        fn consume<R, F>(self, len: usize, f: F) -> R
        where
            F: FnOnce(&mut [u8]) -> R,
        {
            self.consume_token(len, f)
        }
    }

    impl<MODE: WifiDeviceMode> Driver for WifiDevice<'_, MODE> {
        type RxToken<'a> = WifiRxToken<MODE> where Self: 'a;
        type TxToken<'a> = WifiTxToken<MODE> where Self: 'a;

        fn receive(
            &mut self,
            cx: &mut core::task::Context,
        ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
            self.mode.register_receive_waker(cx);
            self.mode.register_transmit_waker(cx);
            self.mode.rx_token()
        }

        fn transmit(&mut self, cx: &mut core::task::Context) -> Option<Self::TxToken<'_>> {
            self.mode.register_transmit_waker(cx);
            self.mode.tx_token()
        }

        fn link_state(&mut self, cx: &mut core::task::Context) -> embassy_net_driver::LinkState {
            self.mode.register_link_state_waker(cx);
            self.mode.link_state()
        }

        fn capabilities(&self) -> Capabilities {
            let mut caps = Capabilities::default();
            caps.max_transmission_unit = MTU;
            caps.max_burst_size = if crate::CONFIG.max_burst_size == 0 {
                None
            } else {
                Some(crate::CONFIG.max_burst_size)
            };
            caps
        }

        fn hardware_address(&self) -> HardwareAddress {
            HardwareAddress::Ethernet(self.mac_address())
        }
    }
}

#[cfg(feature = "async")]
mod asynch {
    use core::task::Poll;

    use embassy_sync::waitqueue::AtomicWaker;
    use num_traits::FromPrimitive;

    use super::*;

    // TODO assumes STA mode only
    impl<'d> WifiController<'d> {
        /// Async version of [`embedded_svc::wifi::Wifi`]'s `scan_n` method
        pub async fn scan_n<const N: usize>(
            &mut self,
        ) -> Result<(heapless::Vec<AccessPointInfo, N>, usize), WifiError> {
            self.scan_with_config(Default::default()).await
        }

        pub async fn scan_with_config<const N: usize>(
            &mut self,
            config: ScanConfig<'_>,
        ) -> Result<(heapless::Vec<AccessPointInfo, N>, usize), WifiError> {
            Self::clear_events(WifiEvent::ScanDone);
            esp_wifi_result!(wifi_start_scan(false, config))?;

            // Prevents memory leak if `scan_n`'s future is dropped.
            let guard = FreeApListOnDrop;
            WifiEventFuture::new(WifiEvent::ScanDone).await;

            guard.defuse();

            let count = self.scan_result_count()?;
            let result = self.scan_results()?;

            Ok((result, count))
        }

        /// Async version of [`embedded_svc::wifi::Wifi`]'s `start` method
        pub async fn start(&mut self) -> Result<(), WifiError> {
            let mode = WifiMode::try_from(&self.config)?;

            let mut events = enumset::enum_set! {};
            if mode.is_ap() {
                events |= WifiEvent::ApStart;
            }
            if mode.is_sta() {
                events |= WifiEvent::StaStart;
            }

            Self::clear_events(events);

            wifi_start()?;

            self.wait_for_all_events(events, false).await;

            Ok(())
        }

        /// Async version of [`embedded_svc::wifi::Wifi`]'s `stop` method
        pub async fn stop(&mut self) -> Result<(), WifiError> {
            let mode = WifiMode::try_from(&self.config)?;

            let mut events = enumset::enum_set! {};
            if mode.is_ap() {
                events |= WifiEvent::ApStop;
            }
            if mode.is_sta() {
                events |= WifiEvent::StaStop;
            }

            Self::clear_events(events);

            embedded_svc::wifi::Wifi::stop(self)?;

            self.wait_for_all_events(events, false).await;

            reset_ap_state();
            reset_sta_state();

            Ok(())
        }

        /// Async version of [`embedded_svc::wifi::Wifi`]'s `connect` method
        pub async fn connect(&mut self) -> Result<(), WifiError> {
            Self::clear_events(WifiEvent::StaConnected | WifiEvent::StaDisconnected);

            let err = embedded_svc::wifi::Wifi::connect(self).err();

            if MultiWifiEventFuture::new(WifiEvent::StaConnected | WifiEvent::StaDisconnected)
                .await
                .contains(WifiEvent::StaDisconnected)
            {
                Err(err.unwrap_or(WifiError::Disconnected))
            } else {
                Ok(())
            }
        }

        /// Async version of [`embedded_svc::wifi::Wifi`]'s `Disconnect` method
        pub async fn disconnect(&mut self) -> Result<(), WifiError> {
            Self::clear_events(WifiEvent::StaDisconnected);
            embedded_svc::wifi::Wifi::disconnect(self)?;
            WifiEventFuture::new(WifiEvent::StaDisconnected).await;

            Ok(())
        }

        fn clear_events(events: impl Into<EnumSet<WifiEvent>>) {
            critical_section::with(|cs| WIFI_EVENTS.borrow_ref_mut(cs).remove_all(events.into()));
        }

        /// Wait for one [`WifiEvent`].
        pub async fn wait_for_event(&mut self, event: WifiEvent) {
            Self::clear_events(event);
            WifiEventFuture::new(event).await
        }

        /// Wait for one of multiple [`WifiEvent`]s. Returns the events that occurred while waiting.
        pub async fn wait_for_events(
            &mut self,
            events: EnumSet<WifiEvent>,
            clear_pending: bool,
        ) -> EnumSet<WifiEvent> {
            if clear_pending {
                Self::clear_events(events);
            }
            MultiWifiEventFuture::new(events).await
        }

        /// Wait for multiple [`WifiEvent`]s.
        pub async fn wait_for_all_events(
            &mut self,
            mut events: EnumSet<WifiEvent>,
            clear_pending: bool,
        ) {
            if clear_pending {
                Self::clear_events(events);
            }

            while !events.is_empty() {
                let fired = MultiWifiEventFuture::new(events).await;
                events -= fired;
            }
        }
    }

    impl WifiEvent {
        pub(crate) fn waker(&self) -> &'static AtomicWaker {
            match self {
                WifiEvent::ScanDone => {
                    static WAKER: AtomicWaker = AtomicWaker::new();
                    &WAKER
                }
                WifiEvent::StaStart => {
                    static WAKER: AtomicWaker = AtomicWaker::new();
                    &WAKER
                }
                WifiEvent::StaConnected => {
                    static WAKER: AtomicWaker = AtomicWaker::new();
                    &WAKER
                }
                WifiEvent::StaDisconnected => {
                    static WAKER: AtomicWaker = AtomicWaker::new();
                    &WAKER
                }
                WifiEvent::StaStop => {
                    static WAKER: AtomicWaker = AtomicWaker::new();
                    &WAKER
                }
                WifiEvent::WifiReady => {
                    static WAKER: AtomicWaker = AtomicWaker::new();
                    &WAKER
                }
                WifiEvent::StaAuthmodeChange => {
                    static WAKER: AtomicWaker = AtomicWaker::new();
                    &WAKER
                }
                WifiEvent::StaWpsErSuccess => {
                    static WAKER: AtomicWaker = AtomicWaker::new();
                    &WAKER
                }
                WifiEvent::StaWpsErFailed => {
                    static WAKER: AtomicWaker = AtomicWaker::new();
                    &WAKER
                }
                WifiEvent::StaWpsErTimeout => {
                    static WAKER: AtomicWaker = AtomicWaker::new();
                    &WAKER
                }
                WifiEvent::StaWpsErPin => {
                    static WAKER: AtomicWaker = AtomicWaker::new();
                    &WAKER
                }
                WifiEvent::StaWpsErPbcOverlap => {
                    static WAKER: AtomicWaker = AtomicWaker::new();
                    &WAKER
                }
                WifiEvent::ApStart => {
                    static WAKER: AtomicWaker = AtomicWaker::new();
                    &WAKER
                }
                WifiEvent::ApStop => {
                    static WAKER: AtomicWaker = AtomicWaker::new();
                    &WAKER
                }
                WifiEvent::ApStaconnected => {
                    static WAKER: AtomicWaker = AtomicWaker::new();
                    &WAKER
                }
                WifiEvent::ApStadisconnected => {
                    static WAKER: AtomicWaker = AtomicWaker::new();
                    &WAKER
                }
                WifiEvent::ApProbereqrecved => {
                    static WAKER: AtomicWaker = AtomicWaker::new();
                    &WAKER
                }
                WifiEvent::FtmReport => {
                    static WAKER: AtomicWaker = AtomicWaker::new();
                    &WAKER
                }
                WifiEvent::StaBssRssiLow => {
                    static WAKER: AtomicWaker = AtomicWaker::new();
                    &WAKER
                }
                WifiEvent::ActionTxStatus => {
                    static WAKER: AtomicWaker = AtomicWaker::new();
                    &WAKER
                }
                WifiEvent::RocDone => {
                    static WAKER: AtomicWaker = AtomicWaker::new();
                    &WAKER
                }
                WifiEvent::StaBeaconTimeout => {
                    static WAKER: AtomicWaker = AtomicWaker::new();
                    &WAKER
                }
            }
        }
    }

    pub(crate) struct WifiEventFuture {
        event: WifiEvent,
    }

    impl WifiEventFuture {
        pub fn new(event: WifiEvent) -> Self {
            Self { event }
        }
    }

    impl core::future::Future for WifiEventFuture {
        type Output = ();

        fn poll(
            self: core::pin::Pin<&mut Self>,
            cx: &mut core::task::Context<'_>,
        ) -> Poll<Self::Output> {
            self.event.waker().register(cx.waker());
            if critical_section::with(|cs| WIFI_EVENTS.borrow_ref_mut(cs).remove(self.event)) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }
    }

    pub(crate) struct MultiWifiEventFuture {
        event: EnumSet<WifiEvent>,
    }

    impl MultiWifiEventFuture {
        pub fn new(event: EnumSet<WifiEvent>) -> Self {
            Self { event }
        }
    }

    impl core::future::Future for MultiWifiEventFuture {
        type Output = EnumSet<WifiEvent>;

        fn poll(
            self: core::pin::Pin<&mut Self>,
            cx: &mut core::task::Context<'_>,
        ) -> Poll<Self::Output> {
            let output = critical_section::with(|cs| {
                let mut events = WIFI_EVENTS.borrow_ref_mut(cs);
                let active = events.intersection(self.event);
                events.remove_all(active);
                active
            });
            if output.is_empty() {
                for event in self.event.iter() {
                    event.waker().register(cx.waker());
                }

                Poll::Pending
            } else {
                Poll::Ready(output)
            }
        }
    }
}

struct FreeApListOnDrop;
impl FreeApListOnDrop {
    pub fn defuse(self) {
        core::mem::forget(self);
    }
}

impl Drop for FreeApListOnDrop {
    fn drop(&mut self) {
        unsafe {
            include::esp_wifi_clear_ap_list();
        }
    }
}
