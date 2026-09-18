//! Helper for string-valued enums that stay forward compatible.
//!
//! Every such enum carries an `Other(String)` catch-all so that a client built
//! against schema v1 can still read a registry that has started using a value
//! introduced later (§7). Code that must reject unknown values does so
//! explicitly during validation, where it can produce a good error message,
//! rather than failing at parse time with a serde error.

/// Declares a string-serialised enum with an `Other(String)` catch-all.
macro_rules! string_enum {
    (
        $(#[$meta:meta])*
        pub enum $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident => $repr:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum $name {
            $( $(#[$vmeta])* $variant, )+
            /// A value this build does not know about. Preserved verbatim.
            Other(String),
        }

        impl $name {
            /// Every value known to this build, in declaration order.
            pub const KNOWN: &'static [$name] = &[ $( $name::$variant ),+ ];

            /// The wire representation.
            pub fn as_str(&self) -> &str {
                match self {
                    $( $name::$variant => $repr, )+
                    $name::Other(raw) => raw.as_str(),
                }
            }

            /// Whether this build understands the value.
            pub fn is_known(&self) -> bool {
                !matches!(self, $name::Other(_))
            }

            /// The known values, formatted for an error message.
            pub fn known_values() -> String {
                let all: Vec<&str> = vec![ $( $repr ),+ ];
                all.join(", ")
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl std::str::FromStr for $name {
            type Err = std::convert::Infallible;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(match s {
                    $( $repr => $name::$variant, )+
                    other => $name::Other(other.to_owned()),
                })
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
                ser.serialize_str(self.as_str())
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(de)?;
                Ok(raw.parse().expect("parsing is infallible"))
            }
        }

        impl schemars::JsonSchema for $name {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                std::borrow::Cow::Borrowed(stringify!($name))
            }
            fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
                schemars::json_schema!({
                    "type": "string",
                    "enum": [ $( $repr ),+ ],
                })
            }
        }
    };
}

pub(crate) use string_enum;
