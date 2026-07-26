//! Manual add-host-by-IP entry state.
//!
//! Split out of the former single-file `ui.rs`; see `super`'s module docs.

/// punktfunk's conventional host port (see `store::dev_override_connect`'s
/// fallback) — fixed and not user-editable, so the add-host screen only ever
/// has to ask for an IP address.
pub const FIXED_HOST_PORT: u16 = 9777;

/// Manual "add host by IP" entry state: a plain, naturally-growing digit
/// string rather than a fixed-width masked grid — no `_` placeholders, no
/// per-octet box, no port field (that's always [`FIXED_HOST_PORT`]). Dots are
/// inserted automatically once an octet is complete (three digits, or a
/// fourth that would push its value past 255), so the Magic Remote's number
/// pad (`digit_key_value`) — the only realistic input this screen gets — is
/// enough on its own, with Left/Right (see `app.rs`'s `handle_add_host_event`)
/// standing in for backspace/"next octet" on a remote with no dot key.
#[derive(Default)]
pub struct AddHostState {
    /// Completed octets so far (0-3 of them once a further one is being typed).
    octets: Vec<u8>,
    /// Digits typed into the octet currently being entered, not yet finalized
    /// into `octets` — kept as text (not a parsed `u8`) so it can grow one
    /// digit at a time and still show a partial value like "2" or "25".
    current: String,
}

impl AddHostState {
    /// Pre-fills from an existing dotted-quad address, for `Screen::EditHost`. A
    /// value that isn't four numeric octets comes back empty rather than partially
    /// parsed — better to retype than to silently edit a mangled address.
    pub fn from_ip(ip: &str) -> Self {
        let parts: Vec<&str> = ip.split('.').collect();
        if parts.len() != 4 {
            return Self::default();
        }
        let mut octets = Vec::with_capacity(4);
        for p in parts {
            match p.parse::<u8>() {
                Ok(v) => octets.push(v),
                Err(_) => return Self::default(),
            }
        }
        Self {
            octets,
            current: String::new(),
        }
    }

    /// Types one character from the webOS on-screen keyboard (`Event::TextInput`,
    /// see `main.rs`) — digits behave exactly as the remote's number pad does, and a
    /// literal `.` finishes the current octet, since a real keyboard *does* have the
    /// dot key the Magic Remote lacks. Anything else is ignored: this field only ever
    /// holds an IPv4 address.
    pub fn enter_char(&mut self, c: char) {
        if let Some(d) = c.to_digit(10) {
            self.enter_digit(d as u8);
        } else if c == '.' {
            self.advance_octet();
        }
    }

    /// Whether exactly four octets' worth of digits have been typed — the
    /// point at which `host_and_port()` names a real, connectable address.
    pub fn is_complete(&self) -> bool {
        (self.octets.len() == 4 && self.current.is_empty()) || (self.octets.len() == 3 && !self.current.is_empty())
    }

    pub fn host_and_port(&self) -> (String, u16) {
        let mut parts: Vec<String> = self.octets.iter().map(u8::to_string).collect();
        if !self.current.is_empty() {
            parts.push(self.current.clone());
        }
        (parts.join("."), FIXED_HOST_PORT)
    }

    /// What's actually been typed so far, exactly as typed — no mask, no
    /// placeholders, no port.
    pub fn display_text(&self) -> String {
        self.host_and_port().0
    }

    /// Types one digit (0-9) into the octet currently being entered, finishing
    /// it automatically (a dot appears) once it hits three digits or a fourth
    /// digit would push its value past 255 — the same auto-advance idiom as a
    /// phone's IP-entry field, needed since the remote has no dot key of its own.
    pub fn enter_digit(&mut self, digit: u8) {
        if self.octets.len() >= 4 {
            return;
        }
        let mut candidate = self.current.clone();
        candidate.push((b'0' + digit) as char);
        let value: u32 = candidate.parse().unwrap_or(0);
        if value > 255 || candidate.len() > 3 {
            self.advance_octet();
            if self.octets.len() < 4 {
                self.current.push((b'0' + digit) as char);
            }
            return;
        }
        self.current = candidate;
        if self.current.len() == 3 {
            self.advance_octet();
        }
    }

    /// Deletes the last typed character — a digit from the in-progress octet,
    /// or (once that's empty) undoes the last completed octet back into it for
    /// editing. Left on the d-pad.
    pub fn backspace(&mut self) {
        if !self.current.is_empty() {
            self.current.pop();
        } else if let Some(last) = self.octets.pop() {
            self.current = last.to_string();
        }
    }

    /// Manually finishes the octet in progress — so e.g. "8" can become
    /// "8.8.8.8" without waiting for three digits or an overflow. Right on the
    /// d-pad, standing in for the "." key a real keyboard would have.
    pub fn advance_octet(&mut self) {
        if self.current.is_empty() || self.octets.len() >= 4 {
            return;
        }
        let value: u8 = self.current.parse().unwrap_or(0);
        self.octets.push(value);
        self.current.clear();
    }
}
