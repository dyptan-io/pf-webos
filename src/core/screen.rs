#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Screen {
    Home,
    Pairing,
    Settings,
    AddHost,
    Wake,
    ForgetHost,
    HostMenu,
    EditHost,
    About,
    SpeedTest,
    WakeSettings,
    PinLimit,
    /// Log level debug aid (see `app/diagnostics.rs`).
    Diagnostics,
    /// Experimental/unstable toggles (see `app/experimental.rs`).
    Experimental,
    /// "Send logs to developer" confirmation (see `app/sendlogs.rs`).
    SendLogs,
}

/// Pairing modal's focused input: PIN row or "Request access" button.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PairingFocus {
    Pin,
    RequestAccess,
}

/// Home screen focus location.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HomeFocus {
    Sidebar(usize),
    SidebarMenu(usize),
    Grid(usize),
}
