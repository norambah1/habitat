// Copyright (c) 2016-2017 Chef Software Inc. and/or applicable contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

/// Collect all the configuration data that is exposed to users, and render it.

use std;
use std::ascii::AsciiExt;
use std::env;
use std::fs::File;
use std::io::prelude::*;
use std::path::{Path, PathBuf};
use std::result;

use fs;
use hcore::crypto;
use serde::{Serialize, Serializer};
use serde::ser::SerializeMap;
use serde_json;
use toml;

use super::Pkg;
use census::CensusGroup;
use error::{Error, Result};
use templating::{TemplateRenderer, RenderContext};

static LOGKEY: &'static str = "CF";
static ENV_VAR_PREFIX: &'static str = "HAB";
/// The maximum TOML table merge depth allowed before failing the operation. The value here is
/// somewhat arbitrary (stack size cannot be easily computed beforehand and different libc
/// implementations will impose different size constraints), however a parallel data structure that
/// is deeper than this value crosses into overly complex territory when describing configuration
/// for a single service.
static TOML_MAX_MERGE_DEPTH: u16 = 30;

/// Trait for getting paths to directories where various configuration
/// files are expected to be.
pub trait PackageConfigPaths {
    /// Get name of the package (basically name part of package ident.
    fn name(&self) -> String;
    /// Get path to directory which holds default.toml.
    fn default_config_dir(&self) -> PathBuf;
    /// Get recommended path to directory which holds user.toml.
    fn recommended_user_config_dir(&self) -> PathBuf;
    /// Get deprecated path to directory which holds user.toml.
    fn deprecated_user_config_dir(&self) -> PathBuf;
}

impl PackageConfigPaths for Pkg {
    fn name(&self) -> String {
        self.name.clone()
    }
    fn default_config_dir(&self) -> PathBuf {
        self.path.clone()
    }
    fn recommended_user_config_dir(&self) -> PathBuf {
        fs::user_config_path(&self.name)
    }
    fn deprecated_user_config_dir(&self) -> PathBuf {
        self.svc_path.clone()
    }
}

#[derive(Clone, Debug)]
pub struct Cfg {
    /// Default level configuration loaded by a Package's `default.toml`
    pub default: Option<toml::Value>,
    /// User level configuration loaded by a Service's `user.toml`
    pub user: Option<toml::Value>,
    /// Gossip level configuration loaded by a census group
    pub gossip: Option<toml::Value>,
    /// Environment level configuration loaded by the Supervisor's process environment
    pub environment: Option<toml::Value>,

    /// Last known incarnation number of the census group's service config
    gossip_incarnation: u64,
}

impl Cfg {
    pub fn new<P: PackageConfigPaths>(package: &P, config_from: Option<&PathBuf>) -> Result<Cfg> {
        let pkg_root = config_from.and_then(|p| Some(p.clone())).unwrap_or(
            package.default_config_dir(),
        );
        let default = Self::load_default(pkg_root)?;
        let user_config_path = Self::determine_user_config_path(package);
        let user = Self::load_user(&user_config_path)?;
        let environment = Self::load_environment(package)?;
        return Ok(Self {
            default: default,
            user: user,
            gossip: None,
            environment: environment,
            gossip_incarnation: 0,
        });
    }

    /// Updates the service configuration with data from a census group if the census group has
    /// newer data than the current configuration.
    ///
    /// Returns true if the configuration was updated.
    pub fn update(&mut self, census_group: &CensusGroup) -> bool {
        match census_group.service_config {
            Some(ref config) => {
                if config.incarnation <= self.gossip_incarnation {
                    return false;
                }
                self.gossip_incarnation = config.incarnation;
                self.gossip = Some(config.value.clone());
                true
            }
            None => false,
        }
    }

    /// Returns a subset of the overall configuration whitelisted by the given package's exports.
    pub fn to_exported(&self, pkg: &Pkg) -> Result<toml::value::Table> {
        let mut map = toml::value::Table::default();
        let cfg = toml::Value::try_from(&self).unwrap();
        for (key, path) in pkg.exports.iter() {
            let fields: Vec<&str> = path.split('.').collect();
            let mut curr = &cfg;
            let mut found = false;

            // JW TODO: the TOML library only provides us with a
            // function to retrieve a value with a path which returns a
            // reference. We actually want the value for ourselves.
            // Let's improve this later to avoid allocating twice.
            for field in fields {
                match curr.get(field) {
                    Some(val) => {
                        curr = val;
                        found = true;
                    }
                    None => found = false,
                }
            }

            if found {
                map.insert(key.clone(), curr.clone());
            }
        }
        Ok(map)
    }

    fn load_toml_file<T1: AsRef<Path>, T2: AsRef<Path>>(
        dir: T1,
        file: T2,
    ) -> Result<Option<toml::Value>> {
        let filename = file.as_ref();
        let path = dir.as_ref().join(&filename);
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(e) => {
                debug!(
                    "Failed to open '{}', {}, {}",
                    filename.display(),
                    path.display(),
                    e
                );
                return Ok(None);
            }
        };
        let mut config = String::new();
        match file.read_to_string(&mut config) {
            Ok(_) => {
                let toml = toml::de::from_str(&config).map_err(|e| {
                    sup_error!(Error::TomlParser(e))
                })?;
                Ok(Some(toml::Value::Table(toml)))
            }
            Err(e) => {
                outputln!(
                    "Failed to read '{}', {}, {}",
                    filename.display(),
                    path.display(),
                    e
                );
                Ok(None)
            }
        }
    }

    fn load_default<T: AsRef<Path>>(config_from: T) -> Result<Option<toml::Value>> {
        Self::load_toml_file(config_from, "default.toml")
    }

    fn determine_user_config_path<P: PackageConfigPaths>(package: &P) -> PathBuf {
        let recommended_dir = package.recommended_user_config_dir();
        let recommended_path = recommended_dir.join("user.toml");
        if recommended_path.exists() {
            return recommended_dir;
        }
        debug!(
            "'user.toml' at {} does not exist",
            recommended_path.display()
        );
        let deprecated_dir = package.deprecated_user_config_dir();
        let deprecated_path = deprecated_dir.join("user.toml");
        if deprecated_path.exists() {
            outputln!(
                "The user configuration location at {} is deprecated, \
                 consider putting it in {}",
                deprecated_path.display(),
                recommended_path.display(),
            );
            return deprecated_dir;
        }
        debug!(
            "'user.toml' at {} does not exist",
            deprecated_path.display()
        );
        recommended_dir
    }

    fn load_user<T: AsRef<Path>>(path: T) -> Result<Option<toml::Value>> {
        Self::load_toml_file(path, "user.toml")
    }

    fn load_environment<P: PackageConfigPaths>(package: &P) -> Result<Option<toml::Value>> {
        let var_name = format!("{}_{}", ENV_VAR_PREFIX, package.name())
            .to_ascii_uppercase()
            .replace("-", "_");
        match env::var(&var_name) {
            Ok(config) => {
                match toml::de::from_str(&config) {
                    Ok(toml) => {
                        return Ok(Some(toml::Value::Table(toml)));
                    }
                    Err(err) => debug!("Attempted to parse env config as toml and failed {}", err),
                }
                match serde_json::from_str(&config) {
                    Ok(json) => {
                        return Ok(Some(toml::Value::Table(json)));
                    }
                    Err(err) => debug!("Attempted to parse env config as json and failed {}", err),
                }
                Err(sup_error!(Error::BadEnvConfig(var_name)))
            }
            Err(e) => {
                debug!(
                    "Looking up environment variable {} failed: {:?}",
                    var_name,
                    e
                );
                Ok(None)
            }
        }
    }
}

impl Serialize for Cfg {
    fn serialize<S>(&self, serializer: S) -> result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut table = toml::value::Table::new();
        if let Some(toml::Value::Table(ref default_cfg)) = self.default {
            if let Err(err) = toml_merge(&mut table, default_cfg) {
                outputln!("Error merging default-cfg into config, {}", err);
            }
        }
        if let Some(toml::Value::Table(ref env_cfg)) = self.environment {
            if let Err(err) = toml_merge(&mut table, env_cfg) {
                outputln!("Error merging environment-cfg into config, {}", err);
            }
        }
        if let Some(toml::Value::Table(ref user_cfg)) = self.user {
            if let Err(err) = toml_merge(&mut table, user_cfg) {
                outputln!("Error merging user-cfg into config, {}", err);
            }
        }
        if let Some(toml::Value::Table(ref gossip_cfg)) = self.gossip {
            if let Err(err) = toml_merge(&mut table, gossip_cfg) {
                outputln!("Error merging gossip-cfg into config, {}", err);
            }
        }

        // Be sure to visit non-tables first (and also non
        // array-of-tables) as all keys must be emitted first.
        let mut map = serializer.serialize_map(Some(table.len()))?;
        for (k, v) in &table {
            if !v.is_array() && !v.is_table() {
                map.serialize_key(&k)?;
                map.serialize_value(&v)?;
            }
        }
        for (k, v) in &table {
            if v.is_array() {
                map.serialize_key(&k)?;
                map.serialize_value(&v)?;
            }
        }
        for (k, v) in &table {
            if v.is_table() {
                map.serialize_key(&k)?;
                map.serialize_value(&v)?;
            }
        }
        map.end()
    }
}

#[derive(Debug)]
pub struct CfgRenderer(TemplateRenderer);

impl CfgRenderer {
    pub fn new<T>(templates_path: T) -> Result<Self>
    where
        T: AsRef<Path>,
    {
        let mut template = TemplateRenderer::new();
        if let Ok(entries) = std::fs::read_dir(templates_path) {
            for entry in entries {
                if let Ok(entry) = entry {
                    // Skip any entries in the template directory which aren't files. Currently we
                    // don't support recursing into directories to retrieve templates. If you want
                    // to add that feature, this is largely the function you change.
                    match entry.file_type() {
                        Ok(file_type) => {
                            if !file_type.is_file() {
                                continue;
                            }
                        }
                        Err(_) => continue,
                    }
                    let file = entry.path();
                    let name = entry.file_name().to_string_lossy().into_owned();
                    // JW TODO: This error needs improvement. TemplateFileError is too generic.
                    template.register_template_file(&name, &file).map_err(
                        |err| {
                            sup_error!(Error::TemplateFileError(err))
                        },
                    )?;
                }
            }
        }
        Ok(CfgRenderer(template))
    }

    /// Compile and write all configuration files to the configuration directory.
    pub fn compile(&self, pkg: &Pkg, ctx: &RenderContext) -> Result<bool> {
        // JW TODO: This function is loaded with IO errors that will be converted a Supervisor
        // error resulting in the end-user not knowing what the fuck happned at all. We need to go
        // through this and pipe the service group through to let people know which service is
        // having issues and be more descriptive about what happened.
        let mut changed = false;
        for (template, _) in self.0.get_templates() {
            let compiled = self.0.render(&template, ctx)?;
            let compiled_hash = crypto::hash::hash_string(&compiled);
            let cfg_dest = pkg.svc_config_path.join(&template);
            let file_hash = match crypto::hash::hash_file(&cfg_dest) {
                Ok(file_hash) => file_hash,
                Err(e) => {
                    debug!("Cannot read the file in order to hash it: {}", e);
                    String::new()
                }
            };
            if file_hash.is_empty() {
                debug!(
                    "Configuration {} does not exist; restarting",
                    cfg_dest.display()
                );
                outputln!(preamble ctx.svc.group, "Updated {} {}",
                          template.as_str(),
                          compiled_hash);
                let mut config_file = File::create(&cfg_dest)?;
                config_file.write_all(&compiled.into_bytes())?;
                changed = true
            } else {
                if file_hash == compiled_hash {
                    debug!(
                        "Configuration {} {} has not changed; not restarting.",
                        cfg_dest.display(),
                        file_hash
                    );
                    continue;
                } else {
                    debug!(
                        "Configuration {} has changed; restarting",
                        cfg_dest.display()
                    );
                    outputln!(preamble ctx.svc.group,"Updated {} {}",
                              template.as_str(),
                              compiled_hash);
                    let mut config_file = File::create(&cfg_dest)?;
                    config_file.write_all(&compiled.into_bytes())?;
                    changed = true;
                }
            }
        }
        Ok(changed)
    }
}

// Recursively merges the `other` TOML table into `me`
fn toml_merge(me: &mut toml::value::Table, other: &toml::value::Table) -> Result<()> {
    toml_merge_recurse(me, other, 0)
}

fn toml_merge_recurse(
    me: &mut toml::value::Table,
    other: &toml::value::Table,
    depth: u16,
) -> Result<()> {
    if depth > TOML_MAX_MERGE_DEPTH {
        return Err(sup_error!(Error::TomlMergeError(format!(
            "Max recursive merge depth of {} \
                                                             exceeded.",
            TOML_MAX_MERGE_DEPTH
        ))));
    }

    for (key, other_value) in other.iter() {
        if is_toml_value_a_table(key, me) && is_toml_value_a_table(key, other) {
            let mut me_at_key = match *(me.get_mut(key).expect("Key should exist in Table")) {
                toml::Value::Table(ref mut t) => t,
                _ => {
                    return Err(sup_error!(Error::TomlMergeError(format!(
                        "Value at key {} \
                                                                         should be a Table",
                        &key
                    ))));
                }
            };
            toml_merge_recurse(
                &mut me_at_key,
                other_value.as_table().expect(
                    "TOML Value should be a Table",
                ),
                depth + 1,
            )?;
        } else {
            me.insert(key.clone(), other_value.clone());
        }
    }
    Ok(())
}

fn is_toml_value_a_table(key: &str, table: &toml::value::Table) -> bool {
    match table.get(key) {
        None => return false,
        Some(value) => {
            match value.as_table() {
                Some(_) => return true,
                None => return false,
            }
        }
    }
}

#[cfg(test)]
mod test {
    use std::fs;
    use std::fs::OpenOptions;

    use toml;
    use tempdir::TempDir;

    use super::*;
    use error::Error;

    fn toml_from_str(content: &str) -> toml::value::Table {
        toml::from_str(content).expect(&format!("Content should parse as TOML: {}", content))
    }

    #[test]
    fn merge_with_empty_me_table() {
        let mut me = toml_from_str("");
        let other = toml_from_str(
            r#"
            fruit = "apple"
            veggie = "carrot"
            "#,
        );
        let expected = other.clone();
        toml_merge(&mut me, &other).unwrap();

        assert_eq!(me, expected);
    }

    #[test]
    fn merge_with_empty_other_table() {
        let mut me = toml_from_str(
            r#"
            fruit = "apple"
            veggie = "carrot"
            "#,
        );
        let other = toml_from_str("");
        let expected = me.clone();
        toml_merge(&mut me, &other).unwrap();

        assert_eq!(me, expected);
    }

    #[test]
    fn merge_with_shallow_tables() {
        let mut me = toml_from_str(
            r#"
            fruit = "apple"
            veggie = "carrot"
            awesomeness = 10
            "#,
        );
        let other = toml_from_str(
            r#"
            fruit = "orange"
            awesomeness = 99
            "#,
        );
        let expected = toml_from_str(
            r#"
            fruit = "orange"
            veggie = "carrot"
            awesomeness = 99
            "#,
        );
        toml_merge(&mut me, &other).unwrap();

        assert_eq!(me, expected);
    }

    #[test]
    fn merge_with_differing_value_types() {
        let mut me = toml_from_str(
            r#"
            fruit = "apple"
            veggie = "carrot"
            awesome_things = ["carrots", "kitties", "unicorns"]
            heat = 42
            "#,
        );
        let other = toml_from_str(
            r#"
            heat = "hothothot"
            awesome_things = "habitat"
            "#,
        );
        let expected = toml_from_str(
            r#"
            heat = "hothothot"
            fruit = "apple"
            veggie = "carrot"
            awesome_things = "habitat"
            "#,
        );
        toml_merge(&mut me, &other).unwrap();

        assert_eq!(me, expected);
    }

    #[test]
    fn merge_with_table_values() {
        let mut me = toml_from_str(
            r#"
            frubnub = "foobar"

            [server]
            some-details = "initial"
            port = 1000
            "#,
        );
        let other = toml_from_str(
            r#"
            [server]
            port = 5000
            more-details = "yep"
            "#,
        );
        let expected = toml_from_str(
            r#"
            frubnub = "foobar"

            [server]
            port = 5000
            some-details = "initial"
            more-details = "yep"
            "#,
        );
        toml_merge(&mut me, &other).unwrap();

        assert_eq!(me, expected);
    }

    #[test]
    fn merge_with_deep_table_values() {
        let mut me = toml_from_str(
            r#"
            [a.b.c.d.e.f.g.h.i.j.k.l.m.n.o.p.q.r.s.t.u.v.w.x.y.z.aa.ab.ac.ad]
            stew = "carrot"
            [a.b.c.d.e.f.foxtrot]
            fancy = "fork"
            "#,
        );
        let other = toml_from_str(
            r#"
            [a.b.c.d.e.f.g.h.i.j.k.l.m.n.o.p.q.r.s.t.u.v.w.x.y.z.aa.ab.ac.ad]
            stew = "beef"
            [a.b.c.d.e.f.foxtrot]
            fancy = "feast"
            funny = "farm"
            "#,
        );
        let expected = toml_from_str(
            r#"
            [a.b.c.d.e.f.foxtrot]
            funny = "farm"
            fancy = "feast"
            [a.b.c.d.e.f.g.h.i.j.k.l.m.n.o.p.q.r.s.t.u.v.w.x.y.z.aa.ab.ac.ad]
            stew = "beef"
            "#,
        );
        toml_merge(&mut me, &other).unwrap();

        assert_eq!(me, expected);
    }

    #[test]
    fn merge_with_dangerously_deep_table_values() {
        let mut me = toml_from_str(
            r#"
            [a.b.c.d.e.f.g.h.i.j.k.l.m.n.o.p.q.r.s.t.u.v.w.x.y.z.aa.ab.ac.ad.ae.af]
            stew = "carrot"
            "#,
        );
        let other = toml_from_str(
            r#"
            [a.b.c.d.e.f.g.h.i.j.k.l.m.n.o.p.q.r.s.t.u.v.w.x.y.z.aa.ab.ac.ad.ae.af]
            stew = "beef"
            "#,
        );

        match toml_merge(&mut me, &other) {
            Err(e) => {
                match e.err {
                    Error::TomlMergeError(_) => assert!(true),
                    _ => panic!("Should fail with Error::TomlMergeError"),
                }
            }
            Ok(_) => panic!("Should not complete successfully"),
        }
    }

    struct TestPkg {
        base_path: PathBuf,
    }

    impl TestPkg {
        fn new(tmp: &TempDir) -> Self {
            let pkg = Self { base_path: tmp.path().to_owned() };

            fs::create_dir_all(pkg.default_config_dir()).expect(
                "create deprecated user config dir",
            );
            fs::create_dir_all(pkg.recommended_user_config_dir())
                .expect("create recommended user config dir");
            fs::create_dir_all(pkg.deprecated_user_config_dir())
                .expect("create default config dir");
            pkg
        }
    }

    impl PackageConfigPaths for TestPkg {
        fn name(&self) -> String {
            String::from("testing")
        }
        fn default_config_dir(&self) -> PathBuf {
            self.base_path.join("root")
        }
        fn recommended_user_config_dir(&self) -> PathBuf {
            self.base_path.join("user")
        }
        fn deprecated_user_config_dir(&self) -> PathBuf {
            self.base_path.join("svc")
        }
    }

    struct CfgTestData {
        // We hold tmp here only to make sure that the temporary
        // directory gets deleted at the end of the test.
        #[allow(dead_code)]
        tmp: TempDir,
        pkg: TestPkg,
        rucp: PathBuf,
        ducp: PathBuf,
    }

    impl CfgTestData {
        fn new() -> Self {
            let tmp = TempDir::new("habitat_config_test").expect("create temp dir");
            let pkg = TestPkg::new(&tmp);
            let rucp = pkg.recommended_user_config_dir().join("user.toml");
            let ducp = pkg.deprecated_user_config_dir().join("user.toml");
            Self {
                tmp: tmp,
                pkg: pkg,
                rucp: rucp,
                ducp: ducp,
            }
        }
    }

    fn write_toml<P: AsRef<Path>>(path: &P, text: &str) {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .expect("create toml file");
        file.write_all(text.as_bytes()).expect(
            "write raw toml value",
        );
        file.flush().expect("flush changes in toml file");
    }

    fn toml_value_from_str(text: &str) -> toml::Value {
        toml::Value::Table(toml_from_str(text))
    }

    #[test]
    fn load_deprecated_user_toml() {
        let cfg_data = CfgTestData::new();
        let toml = "foo = 42";
        write_toml(&cfg_data.ducp, toml);
        let cfg = Cfg::new(&cfg_data.pkg, None).expect("create config");

        assert_eq!(cfg.user, Some(toml_value_from_str(toml)));
    }

    #[test]
    fn load_recommended_user_toml() {
        let cfg_data = CfgTestData::new();
        let toml = "foo = 42";
        write_toml(&cfg_data.rucp, toml);
        let cfg = Cfg::new(&cfg_data.pkg, None).expect("create config");

        assert_eq!(cfg.user, Some(toml_value_from_str(toml)));
    }

    #[test]
    fn prefer_recommended_to_deprecated() {
        let cfg_data = CfgTestData::new();
        let toml = "foo = 42";
        write_toml(&cfg_data.rucp, toml);
        write_toml(&cfg_data.ducp, "foo = 13");
        let cfg = Cfg::new(&cfg_data.pkg, None).expect("create config");

        assert_eq!(cfg.user, Some(toml_value_from_str(toml)));
    }

    #[test]
    fn serialize_config() {
        let concrete_path = TempDir::new("habitat_config_test").expect("create temp dir");
        let pkg = TestPkg::new(&concrete_path);
        let mut cfg = Cfg::new(&pkg, None).expect("Could not create config");
        let default_toml = "shards = []\n\n\
                            [datastore]\n\
                            database = \"builder_originsrv\"\n\
                            password = \"\"\n\
                            user = \"hab\"\n";

        cfg.default = Some(toml::Value::Table(
            toml::de::from_str(default_toml).unwrap(),
        ));

        assert_eq!(default_toml, toml::to_string(&cfg).unwrap());
    }
}
