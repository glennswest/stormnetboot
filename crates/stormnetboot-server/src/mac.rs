//! MAC address normalisation.
//!
//! A MAC reaches us from several places that each spell it differently: iPXE's
//! `${net0/mac}` uses colons, DHCP tooling often uses dashes, some BMCs report
//! bare hex, and Cisco-style dotted quads turn up in inventory exports. A host
//! must resolve to the same identity regardless of which one asked, so every
//! MAC is normalised the moment it arrives and stored only in canonical form.

use std::fmt;

/// A MAC in canonical form: lowercase, colon-separated, six octets.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Mac([u8; 6]);

#[derive(Debug, PartialEq, Eq)]
pub enum MacParseError {
    /// Not 12 hex digits once separators are removed.
    WrongLength(usize),
    /// A character that is neither hex nor a recognised separator.
    NotHex(char),
}

impl fmt::Display for MacParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongLength(n) => write!(f, "expected 12 hex digits, found {n}"),
            Self::NotHex(c) => write!(f, "invalid character {c:?} in MAC"),
        }
    }
}

impl std::error::Error for MacParseError {}

impl Mac {
    pub fn parse(input: &str) -> Result<Self, MacParseError> {
        let mut nibbles = [0u8; 12];
        let mut seen = 0usize;

        for ch in input.chars() {
            if matches!(ch, ':' | '-' | '.' | '_' | ' ') {
                continue;
            }
            let digit = ch
                .to_digit(16)
                .ok_or(MacParseError::NotHex(ch))? as u8;
            if seen < nibbles.len() {
                nibbles[seen] = digit;
            }
            seen += 1;
        }

        if seen != 12 {
            return Err(MacParseError::WrongLength(seen));
        }

        let mut octets = [0u8; 6];
        for (i, octet) in octets.iter_mut().enumerate() {
            *octet = (nibbles[i * 2] << 4) | nibbles[i * 2 + 1];
        }
        Ok(Self(octets))
    }

    pub fn octets(&self) -> [u8; 6] {
        self.0
    }
}

impl fmt::Display for Mac {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d, e, g] = self.0;
        write!(f, "{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{g:02x}")
    }
}

impl serde::Serialize for Mac {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for Mac {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Mac::parse(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_spelling_normalises_to_the_same_identity() {
        let canonical = "aa:bb:cc:dd:ee:ff";
        for input in [
            "aa:bb:cc:dd:ee:ff",
            "AA:BB:CC:DD:EE:FF",
            "aa-bb-cc-dd-ee-ff",
            "aabb.ccdd.eeff",
            "aabbccddeeff",
            "AA BB CC DD EE FF",
        ] {
            let mac = Mac::parse(input).expect(input);
            assert_eq!(mac.to_string(), canonical, "parsing {input}");
        }
    }

    #[test]
    fn rejects_wrong_length() {
        assert_eq!(Mac::parse("aa:bb:cc"), Err(MacParseError::WrongLength(6)));
        assert_eq!(
            Mac::parse("aa:bb:cc:dd:ee:ff:00"),
            Err(MacParseError::WrongLength(14))
        );
    }

    #[test]
    fn rejects_non_hex() {
        assert_eq!(Mac::parse("zz:bb:cc:dd:ee:ff"), Err(MacParseError::NotHex('z')));
    }

    #[test]
    fn round_trips_through_serde() {
        let mac = Mac::parse("00:1b:21:3c:4d:5e").unwrap();
        let json = serde_json::to_string(&mac).unwrap();
        assert_eq!(json, "\"00:1b:21:3c:4d:5e\"");
        let back: Mac = serde_json::from_str(&json).unwrap();
        assert_eq!(back, mac);
    }

    #[test]
    fn octets_are_big_endian_order() {
        assert_eq!(
            Mac::parse("01:02:03:04:05:06").unwrap().octets(),
            [1, 2, 3, 4, 5, 6]
        );
    }
}
