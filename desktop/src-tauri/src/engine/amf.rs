//! The subset of AMF0 that RTMP publishing needs (commands and metadata).

#[derive(Clone, Debug, PartialEq)]
pub enum Amf {
    Number(f64),
    Bool(bool),
    Str(String),
    Object(Vec<(String, Amf)>),
    Null,
    Undefined,
    EcmaArray(Vec<(String, Amf)>),
    StrictArray(Vec<Amf>),
}

impl Amf {
    pub fn str(s: &str) -> Amf {
        Amf::Str(s.into())
    }

    pub fn obj(props: &[(&str, Amf)]) -> Amf {
        Amf::Object(
            props
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        )
    }

    pub fn get(&self, key: &str) -> Option<&Amf> {
        match self {
            Amf::Object(p) | Amf::EcmaArray(p) => p.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Amf::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Amf::Number(n) => Some(*n),
            _ => None,
        }
    }
}

fn put_key(out: &mut Vec<u8>, k: &str) {
    out.extend_from_slice(&(k.len() as u16).to_be_bytes());
    out.extend_from_slice(k.as_bytes());
}

fn put_props(out: &mut Vec<u8>, props: &[(String, Amf)]) {
    for (k, v) in props {
        put_key(out, k);
        encode(v, out);
    }
    out.extend_from_slice(&[0, 0, 9]);
}

pub fn encode(v: &Amf, out: &mut Vec<u8>) {
    match v {
        Amf::Number(n) => {
            out.push(0);
            out.extend_from_slice(&n.to_be_bytes());
        }
        Amf::Bool(b) => out.extend_from_slice(&[1, *b as u8]),
        Amf::Str(s) if s.len() > 0xFFFF => {
            out.push(0x0C);
            out.extend_from_slice(&(s.len() as u32).to_be_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        Amf::Str(s) => {
            out.push(2);
            put_key(out, s);
        }
        Amf::Object(p) => {
            out.push(3);
            put_props(out, p);
        }
        Amf::Null => out.push(5),
        Amf::Undefined => out.push(6),
        Amf::EcmaArray(p) => {
            out.push(8);
            out.extend_from_slice(&(p.len() as u32).to_be_bytes());
            put_props(out, p);
        }
        Amf::StrictArray(items) => {
            out.push(0x0A);
            out.extend_from_slice(&(items.len() as u32).to_be_bytes());
            for i in items {
                encode(i, out);
            }
        }
    }
}

pub fn encode_all(values: &[Amf]) -> Vec<u8> {
    let mut out = Vec::new();
    for v in values {
        encode(v, &mut out);
    }
    out
}

struct Reader<'a> {
    d: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], String> {
        let end = self.pos + n;
        if end > self.d.len() {
            return Err("AMF: truncated".into());
        }
        let s = &self.d[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn u16(&mut self) -> Result<usize, String> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]) as usize)
    }

    fn u32(&mut self) -> Result<usize, String> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize)
    }

    fn string(&mut self, len: usize) -> Result<String, String> {
        Ok(String::from_utf8_lossy(self.take(len)?).into_owned())
    }

    fn props(&mut self) -> Result<Vec<(String, Amf)>, String> {
        let mut p = Vec::new();
        loop {
            let len = self.u16()?;
            if len == 0 && self.d.get(self.pos) == Some(&9) {
                self.pos += 1;
                return Ok(p);
            }
            let k = self.string(len)?;
            p.push((k, self.value()?));
        }
    }

    fn value(&mut self) -> Result<Amf, String> {
        let marker = self.take(1)?[0];
        Ok(match marker {
            0 => {
                let b = self.take(8)?;
                Amf::Number(f64::from_be_bytes(b.try_into().unwrap()))
            }
            1 => Amf::Bool(self.take(1)?[0] != 0),
            2 => {
                let n = self.u16()?;
                Amf::Str(self.string(n)?)
            }
            3 => Amf::Object(self.props()?),
            5 => Amf::Null,
            6 => Amf::Undefined,
            8 => {
                self.u32()?;
                Amf::EcmaArray(self.props()?)
            }
            0x0A => {
                let n = self.u32()?;
                let mut items = Vec::with_capacity(n.min(64));
                for _ in 0..n {
                    items.push(self.value()?);
                }
                Amf::StrictArray(items)
            }
            0x0C => {
                let n = self.u32()?;
                Amf::Str(self.string(n)?)
            }
            m => return Err(format!("AMF: unsupported marker {m}")),
        })
    }
}

pub fn decode_all(data: &[u8]) -> Result<Vec<Amf>, String> {
    let mut r = Reader { d: data, pos: 0 };
    let mut out = Vec::new();
    while r.pos < data.len() {
        out.push(r.value()?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let v = vec![
            Amf::str("connect"),
            Amf::Number(1.0),
            Amf::obj(&[
                ("app", Amf::str("live2")),
                ("fourCcList", Amf::StrictArray(vec![Amf::str("hvc1")])),
                ("ok", Amf::Bool(true)),
            ]),
            Amf::Null,
            Amf::EcmaArray(vec![("width".into(), Amf::Number(2560.0))]),
        ];
        let bytes = encode_all(&v);
        assert_eq!(decode_all(&bytes).unwrap(), v);
        assert_eq!(v[2].get("app").and_then(Amf::as_str), Some("live2"));
    }
}
