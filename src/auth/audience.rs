use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Audience {
    Client,
    Employee,
    Admin,
}

impl Into<&'static str> for Audience {
    fn into(self) -> &'static str {
        match self {
            Audience::Client => "client",
            Audience::Employee => "employee",
            Audience::Admin => "admin",
        }
    }
}

impl ToString for Audience {
    fn to_string(&self) -> String {
        <Audience as Into<&'static str>>::into(*self).to_string()
    }
}

impl TryFrom<&str> for Audience {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "client" => Ok(Audience::Client),
            "employee" => Ok(Audience::Employee),
            "admin" => Ok(Audience::Admin),
            _ => Err(anyhow::anyhow!("invalid audience string")),
        }
    }
}

impl TryFrom<String> for Audience {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        return Audience::try_from(value.as_str());
    }
}
