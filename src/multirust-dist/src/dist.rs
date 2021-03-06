
use temp;
use errors::*;
use multirust_utils::utils;
use prefix::InstallPrefix;
use manifest::Component;
use manifest::Manifest as ManifestV2;
use manifestation::{Manifestation, UpdateStatus, Changes};
use hyper;

use std::path::Path;
use std::fmt;
use std::env;

use regex::Regex;
use openssl::crypto::hash::{Type, Hasher};
use itertools::Itertools;

pub const DEFAULT_DIST_ROOT: &'static str = "https://static.rust-lang.org/dist";
pub const UPDATE_HASH_LEN: usize = 20;

#[derive(Debug, Clone)]
pub struct PartialToolchainDesc {
    // Either "nightly", "stable", "beta", or an explicit version number
    pub channel: String,
    pub date: Option<String>,
    pub arch: Option<String>,
    pub os: Option<String>,
    pub env: Option<String>,
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct TargetTriple {
    pub arch: String,
    pub os: String,
    pub env: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ToolchainDesc {
    // Either "nightly", "stable", "beta", or an explicit version number
    pub channel: String,
    pub date: Option<String>,
    pub target: TargetTriple,
}

static LIST_ARCHS: &'static [&'static str] = &[
    "i386", "i686", "x86_64", "arm", "armv7", "armv7s", "aarch64", "mips", "mipsel",
    "powerpc", "powerpc64", "powerpc64le"
];
static LIST_OSES: &'static [&'static str] = &[
    "pc-windows", "unknown-linux", "apple-darwin", "unknown-netbsd", "apple-ios",
    "linux", "rumprun-netbsd", "unknown-freebsd"
];
static LIST_ENVS: &'static [&'static str] = &[
    "gnu", "msvc", "gnueabi", "gnueabihf", "androideabi", "musl"
];

impl TargetTriple {
    pub fn from_str(name: &str) -> Result<Self> {
        let pattern = format!(
            r"^({})-({})(?:-({}))?$",
            LIST_ARCHS.join("|"), LIST_OSES.join("|"), LIST_ENVS.join("|")
            );
        let re = Regex::new(&pattern).unwrap();
        re.captures(name).map(|c| {
            fn fn_map(s: &str) -> Option<String> {
                if s == "" {
                    None
                } else {
                    Some(s.to_owned())
                }
            }

            TargetTriple {
                arch: c.at(1).unwrap().to_owned(),
                os: c.at(2).unwrap().to_owned(),
                env: c.at(3).and_then(fn_map),
            }
        }).ok_or(Error::InvalidTargetTriple(name.to_string()))
    }

    pub fn from_host() -> Self {
        let (arch, os, env) = get_host_triple_pieces();
        TargetTriple {
            arch: arch.to_owned(),
            os: os.to_owned(),
            env: env.map(ToOwned::to_owned)
        }
    }
}

impl PartialToolchainDesc {
    pub fn from_str(name: &str) -> Result<Self> {
        let channels = ["nightly", "beta", "stable",
                        r"\d{1}\.\d{1}\.\d{1}",
                        r"\d{1}\.\d{2}\.\d{1}"];

        let pattern = format!(
            r"^({})(?:-(\d{{4}}-\d{{2}}-\d{{2}}))?(?:-({}))?(?:-({}))?(?:-({}))?$",
            channels.join("|"), LIST_ARCHS.join("|"), LIST_OSES.join("|"), LIST_ENVS.join("|")
            );

        let re = Regex::new(&pattern).unwrap();
        re.captures(name).map(|c| {
            fn fn_map(s: &str) -> Option<String> {
                if s == "" {
                    None
                } else {
                    Some(s.to_owned())
                }
            }

            PartialToolchainDesc {
                channel: c.at(1).unwrap().to_owned(),
                date: c.at(2).and_then(fn_map),
                arch: c.at(3).and_then(fn_map),
                os: c.at(4).and_then(fn_map),
                env: c.at(5).and_then(fn_map),
            }
        }).ok_or(Error::InvalidToolchainName(name.to_string()))
    }

    pub fn resolve(self, host: &TargetTriple) -> ToolchainDesc {
        // If OS was specified, don't default to host environment, even if the OS matches
        // the host OS, otherwise cannot specify no environment.
        let env = if self.os.is_some() {
            self.env
        } else {
            self.env.or_else(|| host.env.clone())
        };
        let os = self.os.unwrap_or_else(|| host.os.clone());
        ToolchainDesc {
            channel: self.channel,
            date: self.date,
            target: TargetTriple {
                arch: self.arch.unwrap_or_else(|| host.arch.clone()),
                os: os,
                env: env
            }
        }
    }
}

impl ToolchainDesc {
    pub fn from_str(name: &str) -> Result<Self> {
        let channels = ["nightly", "beta", "stable",
                        r"\d{1}\.\d{1}\.\d{1}",
                        r"\d{1}\.\d{2}\.\d{1}"];

        let pattern = format!(
            r"^({})(?:-(\d{{4}}-\d{{2}}-\d{{2}}))?-({})-({})(?:-({}))?$",
            channels.join("|"), LIST_ARCHS.join("|"), LIST_OSES.join("|"), LIST_ENVS.join("|")
            );

        let re = Regex::new(&pattern).unwrap();
        re.captures(name).map(|c| {
            fn fn_map(s: &str) -> Option<String> {
                if s == "" {
                    None
                } else {
                    Some(s.to_owned())
                }
            }

            ToolchainDesc {
                channel: c.at(1).unwrap().to_owned(),
                date: c.at(2).and_then(fn_map),
                target: TargetTriple {
                    arch: c.at(3).unwrap().to_owned(),
                    os: c.at(4).unwrap().to_owned(),
                    env: c.at(5).and_then(fn_map)
                }
            }
        }).ok_or(Error::InvalidToolchainName(name.to_string()))
    }

    pub fn manifest_v1_url(&self, dist_root: &str) -> String {
        match self.date {
            None => format!("{}/channel-rust-{}", dist_root, self.channel),
            Some(ref date) => format!("{}/{}/channel-rust-{}", dist_root, date, self.channel),
        }
    }

    pub fn manifest_v2_url(&self, dist_root: &str) -> String {
        format!("{}.toml", self.manifest_v1_url(dist_root))
    }
    /// Either "$channel" or "channel-$date"
    pub fn manifest_name(&self) -> String {
        match self.date {
            None => self.channel.clone(),
            Some(ref date) => format!("{}-{}", self.channel, date)
        }
   }

    pub fn package_dir(&self, dist_root: &str) -> String {
        match self.date {
            None => format!("{}", dist_root),
            Some(ref date) => format!("{}/{}", dist_root, date),
        }
    }

    pub fn full_spec(&self) -> String {
        if self.date.is_some() {
            format!("{}", self)
        } else {
            format!("{} (tracking)", self)
        }
    }

    pub fn is_tracking(&self) -> bool {
        self.date.is_none()
    }
}

#[derive(Debug)]
pub struct Manifest<'a>(temp::File<'a>, String);

impl<'a> Manifest<'a> {
    pub fn package_url(&self,
                       package: &str,
                       target_triple: &str,
                       ext: &str)
                       -> Result<Option<String>> {
        let suffix = target_triple.to_owned() + ext;
        Ok(try!(utils::match_file("manifest", &self.0, |line| {
            if line.starts_with(package) && line.ends_with(&suffix) {
                Some(format!("{}/{}", &self.1, line))
            } else {
                None
            }
        })))
    }
}

impl fmt::Display for TargetTriple {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if let Some(ref env) = self.env {
            write!(f, "{}-{}-{}", self.arch, self.os, env)
        } else {
            write!(f, "{}-{}", self.arch, self.os)
        }
    }
}

impl fmt::Display for PartialToolchainDesc {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        try!(write!(f, "{}", &self.channel));

        if let Some(ref date) = self.date {
            try!(write!(f, "-{}", date));
        }
        if let Some(ref arch) = self.arch {
            try!(write!(f, "-{}", arch));
        }
        if let Some(ref os) = self.os {
            try!(write!(f, "-{}", os));
        }
        if let Some(ref env) = self.env {
            try!(write!(f, "-{}", env));
        }

        Ok(())
    }
}

impl fmt::Display for ToolchainDesc {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        try!(write!(f, "{}", &self.channel));

        if let Some(ref date) = self.date {
            try!(write!(f, "-{}", date));
        }
        try!(write!(f, "-{}", self.target));

        Ok(())
    }
}

pub fn download_and_check<'a>(url_str: &str,
                              update_hash: Option<&Path>,
                              ext: &str,
                              cfg: DownloadCfg<'a>)
                              -> Result<Option<(temp::File<'a>, String)>> {
    let hash = try!(download_hash(url_str, cfg));
    let partial_hash: String = hash.chars().take(UPDATE_HASH_LEN).collect();

    if let Some(hash_file) = update_hash {
        if utils::is_file(hash_file) {
            if let Ok(contents) = utils::read_file("update hash", hash_file) {
                if contents == partial_hash {
                    // Skip download, update hash matches
                    return Ok(None);
                }
            } else {
                cfg.notify_handler.call(Notification::CantReadUpdateHash(hash_file));
            }
        } else {
            cfg.notify_handler.call(Notification::NoUpdateHash(hash_file));
        }
    }

    let url = try!(utils::parse_url(url_str));
    let file = try!(cfg.temp_cfg.new_file_with_ext("", ext));

    let mut hasher = Hasher::new(Type::SHA256);
    try!(utils::download_file(url, &file, Some(&mut hasher), ntfy!(&cfg.notify_handler)));
    let actual_hash = hasher.finish()
                            .iter()
                            .map(|b| format!("{:02x}", b))
                            .join("");

    if hash != actual_hash {
        // Incorrect hash
        return Err(Error::ChecksumFailed {
            url: url_str.to_owned(),
            expected: hash,
            calculated: actual_hash,
        });
    } else {
        cfg.notify_handler.call(Notification::ChecksumValid(url_str));
    }

    // TODO: Check the signature of the file

    Ok(Some((file, partial_hash)))
}

#[derive(Copy, Clone, Debug)]
pub struct DownloadCfg<'a> {
    pub dist_root: &'a str,
    pub temp_cfg: &'a temp::Cfg,
    pub notify_handler: NotifyHandler<'a>,
}

pub fn get_host_triple() -> String {
    let (arch, os, maybe_env) = get_host_triple_pieces();
    if let Some(env) = maybe_env {
        format!("{}-{}-{}", arch, os, env)
    } else {
        format!("{}-{}", arch, os)
    }
}

pub fn get_host_triple_pieces() -> (&'static str, &'static str, Option<&'static str>) {
    let arch = match env::consts::ARCH {
        "x86" => "i686", // Why, rust... WHY?
        other => other,
    };

    let os = match env::consts::OS {
        "windows" => "pc-windows",
        "linux" => "unknown-linux",
        "macos" => "apple-darwin",
        _ => unimplemented!()
    };

    let env = match () {
        () if cfg!(target_env = "gnu") => Some("gnu"),
        () if cfg!(target_env = "msvc") => Some("msvc"),
        _ => None,
    };

    (arch, os, env)
}

pub fn get_installer_ext() -> &'static str {
    if cfg!(windows) {
        return ".msi";
    }
    ".tar.gz"
}

pub fn download_hash(url: &str, cfg: DownloadCfg) -> Result<String> {
    let hash_url = try!(utils::parse_url(&(url.to_owned() + ".sha256")));
    let hash_file = try!(cfg.temp_cfg.new_file());

    try!(utils::download_file(hash_url, &hash_file, None, ntfy!(&cfg.notify_handler)));

    Ok(try!(utils::read_file("hash", &hash_file).map(|s| s[0..64].to_owned())))
}

// Installs or updates a toolchain from a dist server. If an initial
// install then it will be installed with the default components. If
// an upgrade then all the existing components will be upgraded.
//
// Returns the manifest's hash if anything changed.
pub fn update_from_dist<'a>(download: DownloadCfg<'a>,
                            update_hash: Option<&Path>,
                            toolchain: &ToolchainDesc,
                            prefix: &InstallPrefix,
                            add: &[Component],
                            remove: &[Component],
                            ) -> Result<Option<String>> {

    let toolchain_str = toolchain.to_string();
    let manifestation = try!(Manifestation::open(prefix.clone(), toolchain.target.clone()));

    let changes = Changes {
        add_extensions: add.to_owned(),
        remove_extensions: remove.to_owned(),
    };

    // TODO: Add a notification about which manifest version is going to be used
    download.notify_handler.call(Notification::DownloadingManifest(&toolchain_str));
    match dl_v2_manifest(download, update_hash, toolchain) {
        Ok(Some((m, hash))) => {
            return match try!(manifestation.update(&m, changes, &download.temp_cfg,
                                                   download.notify_handler.clone())) {
                UpdateStatus::Unchanged => Ok(None),
                UpdateStatus::Changed => Ok(Some(hash)),
            }
        }
        Ok(None) => return Ok(None),
        Err(Error::Utils(::multirust_utils::errors::Error::DownloadingFile {
            error: ::multirust_utils::raw::DownloadError::Status(hyper::status::StatusCode::NotFound),
            ..
        })) => {
            // Proceed to try v1 as a fallback
            download.notify_handler.call(Notification::DownloadingLegacyManifest);
        }
        Err(e) => return Err(e)
    }

    // If the v2 manifest is not found then try v1
    let manifest = try!(dl_v1_manifest(download, toolchain)
                        .map_err(|e| Error::NoManifestFound(toolchain.manifest_name(), Box::new(e))));
    match try!(manifestation.update_v1(&manifest, update_hash,
                                       &download.temp_cfg, download.notify_handler.clone())) {
        None => Ok(None),
        Some(hash) => Ok(Some(hash)),
    }
}

fn dl_v2_manifest<'a>(download: DownloadCfg<'a>,
                      update_hash: Option<&Path>,
                      toolchain: &ToolchainDesc) -> Result<Option<(ManifestV2, String)>> {
    let manifest_url = toolchain.manifest_v2_url(download.dist_root);
    let manifest_dl = try!(download_and_check(&manifest_url,
                                              update_hash, ".toml", download));
    let (manifest_file, manifest_hash) = if let Some(m) = manifest_dl { m } else { return Ok(None) };
    let manifest_str = try!(utils::read_file("manifest", &manifest_file));
    let manifest = try!(ManifestV2::parse(&manifest_str));

    Ok(Some((manifest, manifest_hash)))
}

fn dl_v1_manifest<'a>(download: DownloadCfg<'a>,
                      toolchain: &ToolchainDesc) -> Result<Vec<String>> {
    let root_url = toolchain.package_dir(download.dist_root);

    if !["nightly", "beta", "stable"].contains(&&*toolchain.channel) {
        // This is an explicit version. In v1 there was no manifest,
        // you just know the file to download, so synthesize one.
        let installer_name = format!("{}/rust-{}-{}.tar.gz",
                                     root_url, toolchain.channel, toolchain.target);
        return Ok(vec![installer_name]);
    }

    let manifest_url = toolchain.manifest_v1_url(download.dist_root);
    let manifest_dl = try!(download_and_check(&manifest_url, None, "", download));
    let (manifest_file, _) = manifest_dl.unwrap();
    let manifest_str = try!(utils::read_file("manifest", &manifest_file));
    let urls = manifest_str.lines().map(|s| format!("{}/{}", root_url, s)).collect();

    Ok(urls)
}
