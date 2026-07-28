//! Persistent Linux TUN creation via `/dev/net/tun` ioctls.

use std::fs::OpenOptions;
use std::os::fd::AsRawFd;

use anyhow::{Context, Result, bail};
use libc::c_short;

const IFF_TUN: c_short = 0x0001;
const IFF_NO_PI: c_short = 0x1000;

#[repr(C)]
struct IfReq {
    name: [libc::c_char; 16],
    flags: libc::c_short,
    _pad: [u8; 22],
}

nix::ioctl_write_ptr_bad!(
    tun_set_iff,
    nix::request_code_write!(b'T', 202, std::mem::size_of::<libc::c_int>()),
    IfReq
);
nix::ioctl_write_int_bad!(
    tun_set_persist,
    nix::request_code_write!(b'T', 203, std::mem::size_of::<libc::c_int>())
);
nix::ioctl_write_int_bad!(
    tun_set_owner,
    nix::request_code_write!(b'T', 204, std::mem::size_of::<libc::c_int>())
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Created {
    Fresh,
    Existing,
}

/// Create or attach to a persistent TUN device.
///
/// TUNSETOWNER lets tun2socks attach as the daemon user. Persistence keeps
/// hand-written routes alive across reconnects; it is enabled only after the
/// device and owner are both configured.
pub fn create_persistent(name: &str, owner_uid: u32) -> Result<Created> {
    crate::bind::validate_device_name(name)?;
    let existed = exists(name);
    let file = open_tun()?;
    let request = request(name);
    ioctl_result(
        // SAFETY: `request` has the exact kernel ifreq layout asserted by the
        // test below and remains alive for the duration of the ioctl.
        unsafe { tun_set_iff(file.as_raw_fd(), &request) },
        "creating or attaching to",
        name,
    )?;
    ioctl_result(
        // SAFETY: the fd names the TUN selected above and owner_uid is passed
        // by value as required by TUNSETOWNER.
        unsafe { tun_set_owner(file.as_raw_fd(), owner_uid as libc::c_int) },
        "setting the owner of",
        name,
    )?;
    ioctl_result(
        // SAFETY: the fd still names the selected TUN and 1 enables persist.
        unsafe { tun_set_persist(file.as_raw_fd(), 1) },
        "making persistent",
        name,
    )?;
    Ok(if existed {
        Created::Existing
    } else {
        Created::Fresh
    })
}

/// Remove persistence. The kernel deletes the device after its last fd closes.
pub fn delete(name: &str) -> Result<()> {
    crate::bind::validate_device_name(name)?;
    if !exists(name) {
        bail!("cannot delete TUN device {name:?}: the interface does not exist");
    }
    let file = open_tun()?;
    let request = request(name);
    ioctl_result(
        // SAFETY: see create_persistent; this attaches the fd before changing
        // the persist flag.
        unsafe { tun_set_iff(file.as_raw_fd(), &request) },
        "attaching to",
        name,
    )?;
    ioctl_result(
        // SAFETY: 0 clears persistence on the TUN attached to this fd.
        unsafe { tun_set_persist(file.as_raw_fd(), 0) },
        "removing persistence from",
        name,
    )
    .map(|_| ())
}

pub fn exists(name: &str) -> bool {
    crate::bind::validate_device_name(name).is_ok()
        && std::path::Path::new("/sys/class/net").join(name).exists()
}

/// Does this device look like one oxidom created?
///
/// A persistent TUN deliberately outlives its session, and `down` drops the
/// session record that remembered making it — so the in-memory `created` flag
/// cannot answer this after the first reconnect, and `oxidom tun --down` was
/// left unable to remove its own device. The kernel remembers instead:
/// `TUNSETOWNER` is readable back through sysfs, and only tun/tap devices
/// expose the file at all, so an ordinary interface can never match.
pub fn owned_by(name: &str, uid: u32) -> bool {
    if crate::bind::validate_device_name(name).is_err() {
        return false;
    }
    std::fs::read_to_string(format!("/sys/class/net/{name}/owner"))
        .ok()
        .and_then(|owner| owner.trim().parse::<i64>().ok())
        .is_some_and(|owner| owner >= 0 && owner as u64 == u64::from(uid))
}

fn open_tun() -> Result<std::fs::File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/net/tun")
        .context("opening /dev/net/tun")
}

fn request(name: &str) -> IfReq {
    let mut request = IfReq {
        name: [0; 16],
        flags: IFF_TUN | IFF_NO_PI,
        _pad: [0; 22],
    };
    for (destination, source) in request.name.iter_mut().zip(name.bytes()) {
        *destination = source as libc::c_char;
    }
    request
}

fn ioctl_result<T>(result: nix::Result<T>, operation: &str, name: &str) -> Result<T> {
    match result {
        Ok(value) => Ok(value),
        Err(nix::errno::Errno::EPERM) => bail!(
            "{operation} TUN device {name:?} requires CAP_NET_ADMIN; oxidom will not escalate \
             privileges"
        ),
        Err(error) => Err(error).with_context(|| format!("{operation} TUN device {name:?}")),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn kernel_ifreq_layout_is_exact() {
        assert_eq!(std::mem::size_of::<super::IfReq>(), 40);
    }

    #[test]
    fn absent_devices_are_reported_without_privileges() {
        let name = "oxi-no-such-42";
        assert!(!super::exists(name));
        let error = super::delete(name).unwrap_err().to_string();
        assert!(error.contains("does not exist"), "{error}");
    }

    /// `owner` is a tun/tap-only attribute, so an ordinary interface cannot be
    /// mistaken for one of ours however the uid compares. An unowned tun reads
    /// back as -1, which must not match uid 0 or wrap into a large uid.
    #[test]
    fn ownership_is_only_ever_claimed_for_a_tun_we_own() {
        assert!(!super::owned_by("lo", 0), "lo has no owner attribute");
        assert!(!super::owned_by("oxi-no-such-42", 0));
        assert!(!super::owned_by("../etc/passwd", 0));
        for name in std::fs::read_dir("/sys/class/net")
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
        {
            let Ok(owner) = std::fs::read_to_string(format!("/sys/class/net/{name}/owner")) else {
                continue;
            };
            let claimed = owner.trim() != "-1";
            assert_eq!(
                super::owned_by(&name, owner.trim().parse::<u32>().unwrap_or(u32::MAX)),
                claimed && crate::bind::validate_device_name(&name).is_ok(),
                "{name} reports owner {}",
                owner.trim()
            );
        }
    }

    #[test]
    fn overlong_names_fail_before_opening_tun() {
        let error = super::create_persistent("1234567890123456", 0)
            .unwrap_err()
            .to_string();
        assert!(error.contains("1-15 bytes"), "{error}");
    }
}
