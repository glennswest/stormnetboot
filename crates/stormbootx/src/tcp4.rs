//! A blocking socket over the firmware's own TCP stack.
//!
//! EFI networking is entirely asynchronous: every operation takes a token
//! carrying an event that completes later, and nothing progresses unless the
//! caller keeps invoking `Poll` to give the driver cycles. Forgetting that is
//! the classic way to write an EFI network client that hangs forever with no
//! error.
//!
//! Everything above this wants ordinary blocking reads and writes — the NVMe
//! state machine is hard enough without an executor underneath it — so each
//! call here issues one token and pumps until it retires.
//!
//! Using EFI_TCP4 rather than SNP is deliberate: the firmware already has a
//! tested TCP/IP stack and a driver for whatever NIC is fitted. Carrying our
//! own would mean ARP, IP, and a TCP state machine in a boot binary, which is
//! what makes iPXE the size it is.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::ptr;

use uefi::boot::{self, SearchType};
use uefi::{Guid, Status, guid};
use uefi_raw::protocol::network::tcp4::{
    Tcp4AccessPoint, Tcp4CompletionToken, Tcp4ConfigData, Tcp4ConnectionToken, Tcp4FragmentData,
    Tcp4IoToken, Tcp4Option, Tcp4Packet, Tcp4Protocol, Tcp4ReceiveData, Tcp4TransmitData,
};
use uefi_raw::table::boot::{EventType, Tpl};
use uefi_raw::{Boolean, Ipv4Address};

// uefi-raw declares `fragment_table` as a flexible array member ([_; 0]), so
// the real structures cannot be built by value. These are the same layout with
// exactly one fragment, which is all this client ever sends: one contiguous
// buffer per operation.
#[repr(C)]
struct TxData1 {
    push: Boolean,
    urgent: Boolean,
    data_length: u32,
    fragment_count: u32,
    fragment_table: [Tcp4FragmentData; 1],
}

#[repr(C)]
struct RxData1 {
    urgent: Boolean,
    data_length: u32,
    fragment_count: u32,
    fragment_table: [Tcp4FragmentData; 1],
}

pub const TCP4_SERVICE_BINDING: Guid = guid!("00720665-67eb-4a99-baf7-d3c33a1c7cc9");
pub const TCP4: Guid = guid!("65530bc7-a359-410f-b010-5aadc7ec2b62");

/// EFI_SERVICE_BINDING_PROTOCOL — two calls, and uefi-raw does not model it.
#[repr(C)]
pub struct ServiceBinding {
    pub create_child: unsafe extern "efiapi" fn(*mut Self, *mut uefi_raw::Handle) -> Status,
    pub destroy_child: unsafe extern "efiapi" fn(*mut Self, uefi_raw::Handle) -> Status,
}

/// Open a protocol by GUID, returning the raw interface.
///
/// The uefi crate's typed wrappers require a `Protocol` impl, which a raw
/// vtable like the service binding does not have, so go through HandleProtocol.
pub fn handle_protocol(handle: uefi_raw::Handle, guid: &Guid) -> Option<*mut core::ffi::c_void> {
    let mut iface = ptr::null_mut();
    let st = unsafe {
        let st_ptr = uefi::table::system_table_raw()?;
        let bs = st_ptr.as_ref().boot_services.as_ref()?;
        (bs.handle_protocol)(
            handle,
            guid as *const Guid as *const uefi_raw::Guid,
            &mut iface,
        )
    };
    (st == Status::SUCCESS && !iface.is_null()).then_some(iface)
}

fn new_event() -> Result<uefi_raw::Event, String> {
    unsafe {
        let st = uefi::table::system_table_raw().ok_or("no system table")?;
        let bs = st.as_ref().boot_services.as_ref().ok_or("no boot services")?;
        let mut event: uefi_raw::Event = ptr::null_mut();
        let s = (bs.create_event)(
            EventType::empty(),
            Tpl::APPLICATION,
            None,
            ptr::null_mut(),
            &mut event,
        );
        if s != Status::SUCCESS {
            return Err(format!("CreateEvent failed: {s:?}"));
        }
        Ok(event)
    }
}

fn close_event(event: uefi_raw::Event) {
    unsafe {
        if let Some(st) = uefi::table::system_table_raw() {
            if let Some(bs) = st.as_ref().boot_services.as_ref() {
                (bs.close_event)(event);
            }
        }
    }
}

fn signalled(event: uefi_raw::Event) -> bool {
    unsafe {
        let Some(st) = uefi::table::system_table_raw() else {
            return false;
        };
        let Some(bs) = st.as_ref().boot_services.as_ref() else {
            return false;
        };
        (bs.check_event)(event) == Status::SUCCESS
    }
}

/// Is a usable TCP4 stack present at all?
pub fn available() -> bool {
    boot::locate_handle_buffer(SearchType::ByProtocol(&TCP4_SERVICE_BINDING))
        .map(|h| !h.is_empty())
        .unwrap_or(false)
}

pub struct Tcp4Socket {
    sb: *mut ServiceBinding,
    child: uefi_raw::Handle,
    tcp: *mut Tcp4Protocol,
    /// Left over from a receive that returned more than the caller wanted.
    pending: Vec<u8>,
}

impl Tcp4Socket {
    pub fn connect(remote: [u8; 4], port: u16) -> Result<Self, String> {
        let handles = boot::locate_handle_buffer(SearchType::ByProtocol(&TCP4_SERVICE_BINDING))
            .map_err(|e| format!("no EFI_TCP4 service binding: {e:?}"))?;
        let sb_handle = *handles.first().ok_or("no TCP4 service binding handles")?;

        let sb = handle_protocol(sb_handle.as_ptr(), &TCP4_SERVICE_BINDING)
            .ok_or("could not open the TCP4 service binding")?
            as *mut ServiceBinding;

        let mut child: uefi_raw::Handle = ptr::null_mut();
        let st = unsafe { ((*sb).create_child)(sb, &mut child) };
        if st != Status::SUCCESS {
            return Err(format!("TCP4 CreateChild failed: {st:?}"));
        }

        let tcp = match handle_protocol(child, &TCP4) {
            Some(p) => p as *mut Tcp4Protocol,
            None => {
                unsafe { ((*sb).destroy_child)(sb, child) };
                return Err("EFI_TCP4 missing on the new child".to_string());
            }
        };

        let mut sock = Self {
            sb,
            child,
            tcp,
            pending: Vec::new(),
        };
        sock.configure(remote, port)?;
        sock.do_connect()?;
        Ok(sock)
    }

    /// Configure with whatever address the firmware's stack already holds.
    ///
    /// `NO_MAPPING` means the stack is up but has no address yet — DHCP has
    /// not finished. On a cold boot the lease often lands a second or two
    /// after an application starts, so this is a timing condition to wait out,
    /// not a failure to report.
    fn configure(&mut self, remote: [u8; 4], port: u16) -> Result<(), String> {
        for attempt in 0..40 {
            let mut cfg = Tcp4ConfigData {
                type_of_service: 0,
                time_to_live: 64,
                access_point: Tcp4AccessPoint {
                    use_default_address: Boolean::TRUE,
                    station_address: Ipv4Address([0, 0, 0, 0]),
                    subnet_mask: Ipv4Address([0, 0, 0, 0]),
                    station_port: 0,
                    remote_address: Ipv4Address(remote),
                    remote_port: port,
                    active_flag: Boolean::TRUE,
                },
                control_option: ptr::null_mut::<Tcp4Option>(),
            };
            match unsafe { ((*self.tcp).configure)(self.tcp, &mut cfg) } {
                Status::SUCCESS => return Ok(()),
                Status::NO_MAPPING => {
                    if attempt == 0 {
                        uefi::println!("    waiting for the firmware's DHCP lease...");
                    }
                    boot::stall(core::time::Duration::from_millis(500));
                }
                other => return Err(format!("TCP4 Configure failed: {other:?}")),
            }
        }
        Err("no IP address after 20s (NO_MAPPING)".to_string())
    }

    fn do_connect(&mut self) -> Result<(), String> {
        let event = new_event()?;
        let mut token = Tcp4ConnectionToken {
            completion_token: Tcp4CompletionToken {
                event,
                status: Status::SUCCESS,
            },
        };
        let st = unsafe { ((*self.tcp).connect)(self.tcp, &mut token) };
        if st != Status::SUCCESS {
            close_event(event);
            return Err(format!("TCP4 Connect rejected: {st:?}"));
        }
        let r = self.pump(&token.completion_token, "connect");
        close_event(event);
        r
    }

    /// Drive the stack until a token retires.
    ///
    /// `Poll` is what gives the TCP driver cycles; without it nothing ever
    /// completes and this waits forever.
    fn pump(&self, token: &Tcp4CompletionToken, what: &str) -> Result<(), String> {
        // ~30s at 500us per turn, matching the host client's I/O timeout.
        for _ in 0..60_000 {
            unsafe { ((*self.tcp).poll)(self.tcp) };
            if signalled(token.event) {
                return if token.status == Status::SUCCESS {
                    Ok(())
                } else {
                    Err(format!("{what} failed: {:?}", token.status))
                };
            }
            boot::stall(core::time::Duration::from_micros(500));
        }
        Err(format!("{what} timed out"))
    }

    pub fn send(&mut self, data: &[u8]) -> Result<(), String> {
        if data.is_empty() {
            return Ok(());
        }
        let event = new_event()?;
        let mut tx = TxData1 {
            push: Boolean::TRUE,
            urgent: Boolean::FALSE,
            data_length: data.len() as u32,
            fragment_count: 1,
            fragment_table: [Tcp4FragmentData {
                fragment_length: data.len() as u32,
                fragment_buf: data.as_ptr() as *mut u8,
            }],
        };
        let mut token = Tcp4IoToken {
            completion_token: Tcp4CompletionToken {
                event,
                status: Status::SUCCESS,
            },
            packet: Tcp4Packet {
                tx_data: &mut tx as *mut TxData1 as *mut Tcp4TransmitData,
            },
        };
        let st = unsafe { ((*self.tcp).transmit)(self.tcp, &mut token) };
        if st != Status::SUCCESS {
            close_event(event);
            return Err(format!("TCP4 Transmit rejected: {st:?}"));
        }
        let r = self.pump(&token.completion_token, "transmit");
        close_event(event);
        r
    }

    /// One receive into a fresh buffer. Returns what the stack handed over,
    /// which may be less than asked for.
    fn recv_some(&mut self, want: usize) -> Result<Vec<u8>, String> {
        let mut buf = vec![0u8; want.clamp(1, 65536)];
        let event = new_event()?;
        let mut rx = RxData1 {
            urgent: Boolean::FALSE,
            data_length: buf.len() as u32,
            fragment_count: 1,
            fragment_table: [Tcp4FragmentData {
                fragment_length: buf.len() as u32,
                fragment_buf: buf.as_mut_ptr(),
            }],
        };
        let mut token = Tcp4IoToken {
            completion_token: Tcp4CompletionToken {
                event,
                status: Status::SUCCESS,
            },
            packet: Tcp4Packet {
                rx_data: &mut rx as *mut RxData1 as *mut Tcp4ReceiveData,
            },
        };
        let st = unsafe { ((*self.tcp).receive)(self.tcp, &mut token) };
        if st != Status::SUCCESS {
            close_event(event);
            return Err(format!("TCP4 Receive rejected: {st:?}"));
        }
        let r = self.pump(&token.completion_token, "receive");
        close_event(event);
        r?;
        let n = (rx.data_length as usize).min(buf.len());
        buf.truncate(n);
        Ok(buf)
    }

    /// Read exactly `n` bytes. Every PDU header and payload length in NVMe/TCP
    /// is known in advance, so this is the primitive that layer wants.
    pub fn read_exact(&mut self, n: usize) -> Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(n);
        if !self.pending.is_empty() {
            let take = self.pending.len().min(n);
            out.extend_from_slice(&self.pending[..take]);
            self.pending.drain(..take);
        }
        while out.len() < n {
            let chunk = self.recv_some(n - out.len())?;
            if chunk.is_empty() {
                return Err("connection closed mid-read".to_string());
            }
            out.extend_from_slice(&chunk);
        }
        if out.len() > n {
            self.pending.extend_from_slice(&out[n..]);
            out.truncate(n);
        }
        Ok(out)
    }

    /// Read until the peer closes. Used for one HTTP response; NVMe reads
    /// exact lengths instead.
    pub fn read_to_end(&mut self, limit: usize) -> Result<Vec<u8>, String> {
        let mut out = core::mem::take(&mut self.pending);
        loop {
            match self.recv_some(8192) {
                Ok(chunk) if chunk.is_empty() => break,
                Ok(chunk) => {
                    out.extend_from_slice(&chunk);
                    if out.len() >= limit {
                        break;
                    }
                }
                // A closed connection arrives as an error status, which is the
                // normal end of a `Connection: close` response.
                Err(_) => break,
            }
        }
        Ok(out)
    }
}

impl Drop for Tcp4Socket {
    fn drop(&mut self) {
        unsafe {
            ((*self.tcp).configure)(self.tcp, ptr::null_mut());
            ((*self.sb).destroy_child)(self.sb, self.child);
        }
    }
}
