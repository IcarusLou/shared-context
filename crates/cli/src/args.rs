use std::collections::{BTreeMap, BTreeSet};

use sctx_domain::{Error, ErrorKind, Result};

pub(crate) struct Options {
    values: BTreeMap<String, Vec<String>>,
    switches: BTreeSet<String>,
}

impl Options {
    pub(crate) fn parse(args: &[String], switch_names: &[&str]) -> Result<Self> {
        let switch_names = switch_names.iter().copied().collect::<BTreeSet<_>>();
        let mut values = BTreeMap::<String, Vec<String>>::new();
        let mut switches = BTreeSet::new();
        let mut index = 0;
        while index < args.len() {
            let name = &args[index];
            if !name.starts_with("--") {
                return Err(invalid(format!("unexpected positional argument: {name}")));
            }
            if switch_names.contains(name.as_str()) {
                if !switches.insert(name.clone()) {
                    return Err(invalid(format!("duplicate switch: {name}")));
                }
                index += 1;
                continue;
            }
            let value = args
                .get(index + 1)
                .ok_or_else(|| invalid(format!("missing value for {name}")))?;
            if value.starts_with("--") {
                return Err(invalid(format!("missing value for {name}")));
            }
            values.entry(name.clone()).or_default().push(value.clone());
            index += 2;
        }
        Ok(Self { values, switches })
    }

    pub(crate) fn allow_only(&self, names: &[&str], switches: &[&str]) -> Result<()> {
        let allowed = names.iter().copied().collect::<BTreeSet<_>>();
        if let Some(name) = self
            .values
            .keys()
            .find(|name| !allowed.contains(name.as_str()))
        {
            return Err(invalid(format!("unknown option: {name}")));
        }
        let allowed_switches = switches.iter().copied().collect::<BTreeSet<_>>();
        if let Some(name) = self
            .switches
            .iter()
            .find(|name| !allowed_switches.contains(name.as_str()))
        {
            return Err(invalid(format!("unknown switch: {name}")));
        }
        Ok(())
    }

    pub(crate) fn required(&self, name: &str) -> Result<&str> {
        self.optional(name)?
            .ok_or_else(|| invalid(format!("missing required option {name}")))
    }

    pub(crate) fn optional(&self, name: &str) -> Result<Option<&str>> {
        match self.values.get(name).map(Vec::as_slice) {
            None => Ok(None),
            Some([value]) => Ok(Some(value)),
            Some(_) => Err(invalid(format!("option {name} may only be used once"))),
        }
    }

    pub(crate) fn many(&self, name: &str) -> Vec<&str> {
        self.values.get(name).map_or_else(Vec::new, |values| {
            values.iter().map(String::as_str).collect()
        })
    }

    pub(crate) fn provided(&self, name: &str) -> bool {
        self.values.contains_key(name)
    }

    pub(crate) fn has(&self, name: &str) -> bool {
        self.switches.contains(name)
    }
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}
