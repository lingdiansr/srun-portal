use crate::config::{self, PortalTarget};

pub const OFFICE_PORTAL_URL: &str = "https://net.szu.edu.cn/srun_portal_pc?ac_id=1";
pub const DORM_PORTAL_URL: &str = "http://172.30.255.42/";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkMode {
    Office,
    Dorm,
}

impl NetworkMode {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "office" => Ok(Self::Office),
            "dorm" => Ok(Self::Dorm),
            _ => Err(format!(
                "network must be one of: office, dorm (got {value:?})"
            )),
        }
    }
}

pub fn select_target(mode: NetworkMode) -> Result<PortalTarget, String> {
    match mode {
        NetworkMode::Office => config::parse_portal_url(OFFICE_PORTAL_URL)
            .map(PortalTarget::Srun)
            .map_err(|error| error.to_string()),
        NetworkMode::Dorm => {
            config::parse_portal_target(DORM_PORTAL_URL).map_err(|error| error.to_string())
        }
    }
}

pub fn select_slug(mode: NetworkMode) -> &'static str {
    match mode {
        NetworkMode::Office => OFFICE_PORTAL_URL,
        NetworkMode::Dorm => DORM_PORTAL_URL,
    }
}

#[cfg(test)]
mod tests {
    use super::{select_slug, NetworkMode, DORM_PORTAL_URL, OFFICE_PORTAL_URL};

    #[test]
    fn parses_only_manual_modes() {
        assert_eq!(NetworkMode::parse("office"), Ok(NetworkMode::Office));
        assert_eq!(NetworkMode::parse("dorm"), Ok(NetworkMode::Dorm));
        assert!(NetworkMode::parse("auto").is_err());
    }

    #[test]
    fn manual_modes_have_stable_default_urls() {
        assert_eq!(select_slug(NetworkMode::Office), OFFICE_PORTAL_URL);
        assert_eq!(select_slug(NetworkMode::Dorm), DORM_PORTAL_URL);
    }
}
