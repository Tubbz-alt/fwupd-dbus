#[macro_use]
extern crate bitflags;
#[macro_use]
extern crate cascade;
#[macro_use]
extern crate err_derive;
#[macro_use]
extern crate shrinkwraprs;

mod common;
mod dbus_helpers;
mod device;
mod release;
mod remote;

pub use self::{device::*, release::*, remote::*};

use dbus::{
    self,
    arg::{Arg, Array, Dict, Get, RefArg, Variant},
    BusType, ConnPath, Connection, ConnectionItem, Message, OwnedFd,
};

use dbus::stdintf::org_freedesktop_dbus::Properties;
use progress_streams::ProgressWriter;
use std::{
    borrow::Cow,
    collections::{BTreeSet, HashMap},
    fs::{File, OpenOptions},
    io::{self, Seek, SeekFrom},
    iter::FromIterator,
    os::unix::io::IntoRawFd,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

pub const DBUS_NAME: &str = "org.freedesktop.fwupd";
pub const DBUS_IFACE: &str = "org.freedesktop.fwupd";
pub const DBUS_PATH: &str = "/";

const TIMEOUT: i32 = -1;

pub type DynVariant = Variant<Box<RefArg + 'static>>;
pub type DBusEntry = (String, DynVariant);

bitflags! {
    /// Controls the behavior of the install method.
    pub struct InstallFlags: u8 {
        const OFFLINE         = 1 << 0;
        const ALLOW_REINSTALL = 1 << 1;
        const ALLOW_OLDER     = 1 << 2;
        const FORCE           = 1 << 3;
        const NO_HISTORY      = 1 << 4;
    }
}

/// Describes the status of the daemon.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Status {
    Unknown,
    Idle,
    Loading,
    Decompressing,
    DeviceRestart,
    DeviceWrite,
    Scheduling,
    Downloading,
    DeviceRead,
    DeviceErase,
    WaitingForAuth,
    DeviceBusy,
    Shutdown,
}

impl From<u8> for Status {
    fn from(value: u8) -> Self {
        use self::Status::*;
        match value {
            0 => Unknown,
            1 => Idle,
            2 => Loading,
            3 => Decompressing,
            4 => DeviceRestart,
            5 => DeviceWrite,
            6 => Scheduling,
            7 => Downloading,
            8 => DeviceRead,
            9 => DeviceErase,
            10 => WaitingForAuth,
            11 => DeviceBusy,
            12 => Shutdown,
            _ => {
                eprintln!("status value {} is out of range", value);
                Idle
            }
        }
    }
}

#[derive(Debug)]
pub enum FlashEvent {
    DownloadInitiate(u64),
    DownloadUpdate(usize),
    DownloadComplete,
    VerifyingChecksum,
    FlashInProgress,
}

/// An error that may occur when using the client.
#[derive(Debug, Error)]
pub enum Error {
    #[error(display = "failed to add match on client connection")]
    AddMatch(#[error(cause)] dbus::Error),
    #[error(display = "argument mismatch in {} method", _0)]
    ArgumentMismatch(&'static str, #[error(cause)] dbus::arg::TypeMismatchError),
    #[error(display = "calling {} method failed", _0)]
    Call(&'static str, #[error(cause)] dbus::Error),
    #[error(display = "unable to establish dbus connection")]
    Connection(#[error(cause)] dbus::Error),
    #[error(display = "the remote firmware which was downloaded has an invalid checksum")]
    FirmwareChecksumMismatch,
    #[error(display = "failed to copy firmware file from remote")]
    FirmwareCopy(#[error(case)] io::Error),
    #[error(display = "failed to create firmware file in user cache")]
    FirmwareCreate(#[error(cause)] io::Error),
    #[error(display = "failed to GET firmware file from remote")]
    FirmwareGet(#[error(cause)] reqwest::Error),
    #[error(display = "failed to open firmware file")]
    FirmwareOpen(#[error(cause)] io::Error),
    #[error(display = "failed to read firmware file")]
    FirmwareRead(#[error(cause)] io::Error),
    #[error(display = "failed to seek to beginning of firmware file")]
    FirmwareSeek(#[error(cause)] io::Error),
    #[error(display = "failed to get property for {}", _0)]
    GetProperty(&'static str, #[error(cause)] dbus::Error),
    #[error(display = "failed to create {} method call", _0)]
    NewMethodCall(&'static str, String),
    #[error(display = "remote not found")]
    RemoteNotFound,
}

/// A DBus client for interacting with the fwupd daemon.
#[derive(Shrinkwrap)]
pub struct Client(dbus::Connection);

impl Client {
    pub fn new() -> Result<Self, Error> {
        Connection::get_private(BusType::System).map(Self).map_err(Error::Connection)
    }

    /// Activate a firmware update on the device.
    pub fn activate<D: AsRef<DeviceId>>(&self, id: D) -> Result<(), Error> {
        self.action_method("Activate", id.as_ref().as_ref())
    }

    /// Clears the results of an offline update.
    pub fn clear_results<D: AsRef<DeviceId>>(&self, id: D) -> Result<(), Error> {
        self.action_method("ClearResults", id.as_ref().as_ref())
    }

    /// The version of this daemon.
    pub fn daemon_version(&self) -> Result<Box<str>, Error> {
        self.get_property::<String>("DaemonVersion").map(Box::from)
    }

    /// Gets details about a local firmware file.
    pub fn details<H: IntoRawFd>(
        &self,
        handle: H,
    ) -> Result<Vec<HashMap<String, DynVariant>>, Error> {
        self.get_handle_method("GetDetails", handle)
    }

    /// Gets a list of all the devices that are supported.
    pub fn devices(&self) -> Result<Vec<Device>, Error> { self.get_method("GetDevices") }

    /// Get a list of all the downgrades possible for a specific device.
    pub fn downgrades<D: AsRef<DeviceId>>(&self, device_id: D) -> Result<BTreeSet<Release>, Error> {
        self.get_device_method("GetDowngrades", device_id.as_ref().as_ref())
    }

    /// Update firmware for a `Device` with the firmware specified in a `Release`.
    pub fn update_device_with_release<F: FnMut(FlashEvent)>(
        &self,
        client: &reqwest::Client,
        device: &Device,
        release: &Release,
        mut flags: InstallFlags,
        mut callback: Option<F>,
    ) -> Result<(), Error> {
        let remote = self.remote(release)?;

        // Local and directory remotes already have the firmware.
        let filename: Option<Cow<'_, Path>> = match remote.kind {
            RemoteKind::Local => Some(Cow::Owned(
                Path::new(remote.filename_cache.as_ref())
                    .parent()
                    .expect("remote filename cache without parent")
                    .join(Path::new(release.uri.as_ref())),
            )),
            RemoteKind::Directory => Some(Cow::Borrowed(Path::new(&release.uri[7..]))),
            _ => None,
        };

        if let Some(filename) = filename {
            if let Some(ref mut cb) = callback {
                cb(FlashEvent::FlashInProgress);
            }

            self.install(&device, "(user)", &filename, None::<File>, flags)?;
            return Ok(());
        }

        if let Some(ref mut cb) = callback {
            cb(FlashEvent::DownloadInitiate(release.size));
        }

        // Create URI, substituting if required.
        let uri = remote.firmware_uri(&release.uri);
        let file_path = common::cache_path_from_uri(&uri);
        let mut req_builder = client.get(uri);

        // Set the username and password.
        if let Some(ref username) = remote.username {
            req_builder = req_builder.basic_auth(username, remote.password.as_ref());
        }

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&file_path)
            .map_err(Error::FirmwareCreate)?;

        eprintln!("downloading {} for {}...", release.version, device.name);
        let mut response = req_builder
            .send()
            .map_err(Error::FirmwareGet)?
            .error_for_status()
            .map_err(Error::FirmwareGet)?;

        let result = match callback {
            Some(ref mut callback) => {
                let mut writer = ProgressWriter::new(&mut file, |progress| {
                    callback(FlashEvent::DownloadUpdate(progress))
                });

                let result = io::copy(&mut response, &mut writer);
                callback(FlashEvent::DownloadComplete);
                result
            }
            None => io::copy(&mut response, &mut file),
        };

        result.map_err(Error::FirmwareCopy)?;

        file.seek(SeekFrom::Start(0)).map_err(Error::FirmwareSeek)?;

        if let Some(ref mut cb) = callback {
            cb(FlashEvent::VerifyingChecksum);
        }

        if let Some((checksum, algorithm)) = common::find_best_checksum(&release.checksums) {
            let checksum_matched = common::validate_checksum(&mut file, checksum, algorithm)
                .map_err(Error::FirmwareRead)?;

            if !checksum_matched {
                return Err(Error::FirmwareChecksumMismatch);
            }
        }

        file.seek(SeekFrom::Start(0)).map_err(Error::FirmwareSeek)?;

        if device.only_offline() {
            flags |= InstallFlags::OFFLINE;
        }

        if let Some(ref mut cb) = callback {
            cb(FlashEvent::FlashInProgress);
        }

        self.install(&device, "(user)", &file_path, Some(file), flags)?;

        Ok(())
    }

    /// Gets a list of all the past firmware updates.
    pub fn history<H: IntoRawFd>(&self, handle: H) -> Result<Vec<Device>, Error> {
        self.get_handle_method("GetHistory", handle)
    }

    /// Schedules a firmware to be installed.
    pub fn install<'a, D: AsRef<DeviceId>, H: IntoRawFd>(
        &self,
        id: D,
        reason: &str,
        filename: &Path,
        handle: Option<H>,
        flags: InstallFlags,
    ) -> Result<(), Error> {
        const METHOD: &str = "Install";

        let fd = match handle {
            Some(handle) => handle.into_raw_fd(),
            None => OpenOptions::new()
                .read(true)
                .open(filename)
                .map_err(Error::FirmwareOpen)?
                .into_raw_fd(),
        };

        let filename = filename.as_os_str().to_str().expect("filename is not UTF-8");

        let options: Vec<(&str, DynVariant)> = cascade! {
            opts: Vec::with_capacity(8);
            ..push(("reason", Variant(Box::new(reason.to_owned()) as Box<dyn RefArg>)));
            ..push(("filename", Variant(Box::new(filename.to_owned()) as Box<dyn RefArg>)));
            | if flags.contains(InstallFlags::OFFLINE) {
                opts.push(("offline", Variant(Box::new(true) as Box<dyn RefArg>)));
            };
            | if flags.contains(InstallFlags::ALLOW_OLDER) {
                opts.push(("allow-older", Variant(Box::new(true) as Box<dyn RefArg>)));
            };
            | if flags.contains(InstallFlags::ALLOW_REINSTALL) {
                opts.push(("allow-reinstall", Variant(Box::new(true) as Box<dyn RefArg>)));
            };
            | if flags.contains(InstallFlags::FORCE) {
                opts.push(("force", Variant(Box::new(true) as Box<dyn RefArg>)));
            };
            | if flags.contains(InstallFlags::NO_HISTORY) {
                opts.push(("no-history", Variant(Box::new(true) as Box<dyn RefArg>)));
            };
        };

        let options = Dict::new(options);

        let id: &str = id.as_ref().as_ref();
        let cb = |m: Message| m.append3(id, OwnedFd::new(fd), options);

        self.call_method(METHOD, cb)?;
        Ok(())
    }

    /// Listens for signals from the DBus daemon.
    pub fn listen_signals<'a>(
        &'a self,
        cancellable: Arc<AtomicBool>,
    ) -> impl Iterator<Item = Signal> + 'a {
        fn filter_signal(ci: ConnectionItem) -> Option<Message> {
            if let ConnectionItem::Signal(ci) = ci {
                Some(ci)
            } else {
                None
            }
        }

        fn read_signal<T: FromIterator<DBusEntry>>(
            signal: Message,
            method: &'static str,
        ) -> Result<T, Error> {
            let iter: Dict<String, Variant<Box<RefArg + 'static>>, _> =
                signal.read1().map_err(|why| Error::ArgumentMismatch(method, why))?;

            Ok(T::from_iter(iter))
        }

        self.iter(TIMEOUT)
            .take_while(move |_| cancellable.load(Ordering::SeqCst))
            .filter_map(filter_signal)
            .filter_map(|signal| {
                let signal = match &*signal.member().unwrap() {
                    "Changed" => Ok(Signal::Changed),
                    "DeviceAdded" => read_signal(signal, "DeviceAdded").map(Signal::DeviceAdded),
                    "DeviceChanged" => {
                        read_signal(signal, "DeviceChanged").map(Signal::DeviceChanged)
                    }
                    "DeviceRemoved" => {
                        read_signal(signal, "DeviceRemoved").map(Signal::DeviceRemoved)
                    }
                    "PropertiesChanged" => signal
                        .read3::<String, HashMap<String, DynVariant>, Vec<String>>()
                        .map_err(|why| Error::ArgumentMismatch("PropertiesChanged", why))
                        .map(|values| Signal::PropertiesChanged {
                            interface:   values.0.into(),
                            changed:     values.1,
                            invalidated: values.2,
                        }),
                    _ => return None,
                };

                match signal {
                    Ok(signal) => Some(signal),
                    Err(why) => {
                        eprintln!("signal error: {}", why);
                        None
                    }
                }
            })
    }

    /// Modifies a device in some way.
    pub fn modify_device<D: AsRef<DeviceId>>(
        &self,
        device_id: D,
        key: &str,
        value: &str,
    ) -> Result<(), Error> {
        let device_id: &str = device_id.as_ref().as_ref();
        self.call_method("ModifyDevice", |m| m.append3(device_id, key, value))?;
        Ok(())
    }

    /// Modifies a remote in some way.
    pub fn modify_remote<R: AsRef<RemoteId>>(
        &self,
        remote_id: R,
        key: &str,
        value: &str,
    ) -> Result<(), Error> {
        let remote_id: &str = remote_id.as_ref().as_ref();
        self.call_method("ModifyRemote", |m| m.append3(remote_id, key, value))?;
        Ok(())
    }

    /// The job percentage completion, or 0 for unknown.
    pub fn percentage(&self) -> Result<u8, Error> {
        self.get_property::<u32>("Percentage").map(|v| v as u8)
    }

    /// Gets a list of all the releases for a specific device.
    pub fn releases<D: AsRef<DeviceId>>(&self, device_id: D) -> Result<BTreeSet<Release>, Error> {
        self.get_device_method("GetReleases", device_id.as_ref().as_ref())
    }

    /// Find the remote with the given ID.
    pub fn remote<D: AsRef<RemoteId>>(&self, id: D) -> Result<Remote, Error> {
        self.remotes()?
            .into_iter()
            .find(|remote| &remote.remote_id == id.as_ref())
            .ok_or(Error::RemoteNotFound)
    }

    /// Gets the list of remotes.
    pub fn remotes(&self) -> Result<Vec<Remote>, Error> { self.get_method("GetRemotes") }

    /// Gets the results of an offline update.
    pub fn results<D: AsRef<DeviceId>>(&self, id: D) -> Result<Option<Device>, Error> {
        let id: &str = id.as_ref().as_ref();
        let message = self.call_method("GetResults", |m| m.append1(id))?;
        let iter: Option<Dict<String, Variant<Box<RefArg + 'static>>, _>> = message.get1();
        Ok(iter.map(Device::from_iter))
    }

    /// The daemon status, e.g. `Decompressing`.
    pub fn status(&self) -> Result<Status, Error> {
        self.get_property::<u32>("Status").map(|v| Status::from(v as u8))
    }

    /// If the daemon has been tainted with a third party plugin.
    pub fn tainted(&self) -> Result<bool, Error> { self.get_property::<bool>("Tainted") }

    /// Unlock the device to allow firmware access.
    pub fn unlock<D: AsRef<DeviceId>>(&self, id: D) -> Result<(), Error> {
        self.action_method("Unlock", id.as_ref().as_ref())
    }

    /// Adds AppStream resource information from a session client.
    pub fn update_metadata<D: IntoRawFd, S: IntoRawFd, R: AsRef<RemoteId>>(
        &self,
        remote_id: R,
        data: D,
        signature: S,
    ) -> Result<(), Error> {
        let remote_id: &str = remote_id.as_ref().as_ref();
        let cb = |m: Message| {
            m.append3(
                remote_id,
                OwnedFd::new(data.into_raw_fd()),
                OwnedFd::new(signature.into_raw_fd()),
            )
        };

        self.call_method("UpdateMetadata", cb)?;
        Ok(())
    }

    /// Get a list of all the upgrades possible for a specific device.
    pub fn upgrades<D: AsRef<DeviceId>>(&self, device_id: D) -> Result<BTreeSet<Release>, Error> {
        self.get_device_method("GetUpgrades", device_id.as_ref().as_ref())
    }

    /// Verifies firmware on a device by reading it back and performing
    /// a cryptographic hash, typically SHA1.
    pub fn verify<D: AsRef<DeviceId>>(&self, id: D) -> Result<(), Error> {
        self.action_method("Verify", id.as_ref().as_ref())
    }

    /// Updates the cryptographic hash stored for a device.
    pub fn verify_update<D: AsRef<DeviceId>>(&self, id: D) -> Result<(), Error> {
        self.action_method("VerifyUpdate", id.as_ref().as_ref())
    }

    fn action_method(&self, method: &'static str, id: &str) -> Result<(), Error> {
        self.call_method(method, |m| m.append(id))?;
        Ok(())
    }

    fn get_method<T: FromIterator<DBusEntry>>(
        &self,
        method: &'static str,
    ) -> Result<Vec<T>, Error> {
        let message = self.call_method(method, |m| m)?;
        let iter: Array<Dict<String, Variant<Box<RefArg + 'static>>, _>, _> =
            message.read1().map_err(|why| Error::ArgumentMismatch(method, why))?;

        Ok(iter.map(T::from_iter).collect())
    }

    fn get_device_method<T: FromIterator<DBusEntry>, C: FromIterator<T>>(
        &self,
        method: &'static str,
        device_id: &str,
    ) -> Result<C, Error> {
        let message = self.call_method(method, |m| m.append1(device_id))?;
        let iter: Array<Dict<String, Variant<Box<RefArg + 'static>>, _>, _> =
            message.read1().map_err(|why| Error::ArgumentMismatch(method, why))?;

        Ok(C::from_iter(iter.map(T::from_iter)))
    }

    fn get_handle_method<T: FromIterator<DBusEntry>, H: IntoRawFd>(
        &self,
        method: &'static str,
        handle: H,
    ) -> Result<Vec<T>, Error> {
        let cb = move |m: Message| m.append1(OwnedFd::new(handle.into_raw_fd()));

        let message = self.call_method(method, cb)?;
        let iter: Array<Dict<String, Variant<Box<RefArg + 'static>>, _>, _> =
            message.read1().map_err(|why| Error::ArgumentMismatch(method, why))?;

        Ok(iter.map(T::from_iter).collect())
    }

    fn get_property<T: for<'a> Get<'a> + Arg>(&self, property: &'static str) -> Result<T, Error> {
        self.connection_path()
            .get::<T>(DBUS_NAME, property)
            .map_err(|why| Error::GetProperty(property, why))
    }

    fn call_method<F: FnOnce(Message) -> Message>(
        &self,
        method: &'static str,
        append_args: F,
    ) -> Result<Message, Error> {
        let mut m = Message::new_method_call(DBUS_NAME, DBUS_PATH, DBUS_IFACE, method)
            .map_err(|why| Error::NewMethodCall(method, why))?;

        m = append_args(m);

        self.send_with_reply_and_block(m, TIMEOUT).map_err(|why| Error::Call(method, why))
    }

    fn connection_path<'a>(&'a self) -> ConnPath<'a, &'a Connection> {
        self.with_path(DBUS_NAME, DBUS_PATH, TIMEOUT)
    }
}

/// Signal received by the daemon when listening for signal events with `Client::listen_signals()`.
pub enum Signal {
    /// Some value on the interface or the number of devices or profiles has changed.
    Changed,
    /// A device has been added.
    DeviceAdded(Device),
    /// A device has been changed.
    DeviceChanged(Device),
    /// A device has been removed.
    DeviceRemoved(Device),
    /// Triggers when a property has changed.
    PropertiesChanged {
        interface:   Box<str>,
        changed:     HashMap<String, DynVariant>,
        invalidated: Vec<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn download_remote() -> Remote {
        Remote {
            enabled: true,
            kind: RemoteKind::Download,
            keyring: KeyringKind::GPG,
            firmware_base_uri: Some("https://my.fancy.cdn/".into()),
            uri: Some("https://s3.amazonaws.com/lvfsbucket/downloads/firmware.xml.gz".into()),
            ..Default::default()
        }
    }

    fn nopath_remote() -> Remote {
        Remote {
            enabled: true,
            kind: RemoteKind::Download,
            keyring: KeyringKind::GPG,
            uri: Some("https://s3.amazonaws.com/lvfsbucket/downloads/firmware.xml.gz".into()),
            ..Default::default()
        }
    }

    #[test]
    fn remote_baseuri() {
        let remote = download_remote();
        let firmware_uri = remote.firmware_uri("http://bbc.co.uk/firmware.cab");
        assert_eq!(firmware_uri.to_string().as_str(), "https://my.fancy.cdn/firmware.cab")
    }

    #[test]
    fn remote_nopath() {
        let remote = nopath_remote();
        let firmware_uri = remote.firmware_uri("firmware.cab");
        assert_eq!(
            firmware_uri.to_string().as_str(),
            "https://s3.amazonaws.com/lvfsbucket/downloads/firmware.cab"
        )
    }
}
