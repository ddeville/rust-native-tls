extern crate schannel;

use self::schannel::cert_context::{CertContext, HashAlgorithm};
use self::schannel::cert_store::{CertAdd, CertStore, Memory, PfxImportOptions};
use self::schannel::schannel_cred::{Algorithm, Direction, Protocol, SchannelCred};
use self::schannel::tls_stream;
use std::collections::VecDeque;
use std::error;
use std::fmt;
use std::io;
use std::str;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use {
    CipherSuiteSet, TlsAcceptorBuilder, TlsBulkEncryptionAlgorithm, TlsConnectorBuilder,
    TlsHashAlgorithm, TlsKeyExchangeAlgorithm, TlsSignatureAlgorithm,
};

impl From<TlsKeyExchangeAlgorithm> for Algorithm {
    fn from(other: TlsKeyExchangeAlgorithm) -> Self {
        match other {
            TlsKeyExchangeAlgorithm::Dhe => Algorithm::DhEphem,
            TlsKeyExchangeAlgorithm::Ecdhe => Algorithm::EcdhEphem,
            TlsKeyExchangeAlgorithm::Rsa => Algorithm::RsaKeyx,
            TlsKeyExchangeAlgorithm::__NonExhaustive => unreachable!(),
        }
    }
}

impl From<TlsSignatureAlgorithm> for Algorithm {
    fn from(other: TlsSignatureAlgorithm) -> Self {
        match other {
            TlsSignatureAlgorithm::Dss => Algorithm::DssSign,
            TlsSignatureAlgorithm::Ecdsa => Algorithm::Ecdsa,
            TlsSignatureAlgorithm::Rsa => Algorithm::RsaSign,
            TlsSignatureAlgorithm::__NonExhaustive => unreachable!(),
        }
    }
}

impl From<TlsBulkEncryptionAlgorithm> for Algorithm {
    fn from(other: TlsBulkEncryptionAlgorithm) -> Self {
        match other {
            TlsBulkEncryptionAlgorithm::Aes128 => Algorithm::Aes128,
            TlsBulkEncryptionAlgorithm::Aes256 => Algorithm::Aes256,
            TlsBulkEncryptionAlgorithm::Des => Algorithm::Des,
            TlsBulkEncryptionAlgorithm::Rc2 => Algorithm::Rc2,
            TlsBulkEncryptionAlgorithm::Rc4 => Algorithm::Rc4,
            TlsBulkEncryptionAlgorithm::TripleDes => Algorithm::TripleDes,
            TlsBulkEncryptionAlgorithm::__NonExhaustive => unreachable!(),
        }
    }
}

impl From<TlsHashAlgorithm> for Algorithm {
    fn from(other: TlsHashAlgorithm) -> Self {
        match other {
            TlsHashAlgorithm::Md5 => Algorithm::Md5,
            TlsHashAlgorithm::Sha1 => Algorithm::Sha1,
            TlsHashAlgorithm::Sha256 => Algorithm::Sha256,
            TlsHashAlgorithm::Sha384 => Algorithm::Sha384,
            TlsHashAlgorithm::__NonExhaustive => unreachable!(),
        }
    }
}

fn expand_algorithms(cipher_suites: &CipherSuiteSet) -> Vec<Algorithm> {
    let mut ret = vec![];
    ret.extend(
        cipher_suites
            .key_exchange
            .iter()
            .copied()
            .map(Algorithm::from),
    );
    ret.extend(cipher_suites.signature.iter().copied().map(Algorithm::from));
    ret.extend(
        cipher_suites
            .bulk_encryption
            .iter()
            .copied()
            .map(Algorithm::from),
    );
    ret.extend(cipher_suites.hash.iter().copied().map(Algorithm::from));
    ret
}

const SEC_E_NO_CREDENTIALS: u32 = 0x8009030E;

static PROTOCOLS: &'static [Protocol] = &[
    Protocol::Ssl3,
    Protocol::Tls10,
    Protocol::Tls11,
    Protocol::Tls12,
];

#[derive(Clone)]
struct CacheEntry {
    domain: String,
    expiry: SystemTime,
    credentials: SchannelCred,
}

// Number of credentials to cache.
const CREDENTIAL_CACHE_SIZE: usize = 10;

// Credentials live for 10 minutes.
const CREDENTIAL_TTL: Duration = Duration::from_secs(10 * 60);

fn convert_protocols(min: Option<::Protocol>, max: Option<::Protocol>) -> &'static [Protocol] {
    let mut protocols = PROTOCOLS;
    if let Some(p) = max.and_then(|max| protocols.get(..max as usize)) {
        protocols = p;
    }
    if let Some(p) = min.and_then(|min| protocols.get(min as usize..)) {
        protocols = p;
    }
    protocols
}

pub struct Error(io::Error);

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        error::Error::source(&self.0)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt(&self.0, fmt)
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&self.0, fmt)
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Error {
        Error(error)
    }
}

#[derive(Clone)]
pub struct Identity {
    cert: CertContext,
}

impl Identity {
    pub fn from_pkcs12(buf: &[u8], pass: &str) -> Result<Identity, Error> {
        let store = PfxImportOptions::new().password(pass).import(buf)?;
        let mut identity = None;

        for cert in store.certs() {
            if cert
                .private_key()
                .silent(true)
                .compare_key(true)
                .acquire()
                .is_ok()
            {
                identity = Some(cert);
                break;
            }
        }

        let identity = match identity {
            Some(identity) => identity,
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "No identity found in PKCS #12 archive",
                ).into());
            }
        };

        Ok(Identity { cert: identity })
    }
}

#[derive(Clone)]
pub struct Certificate(CertContext);

impl Certificate {
    pub fn from_der(buf: &[u8]) -> Result<Certificate, Error> {
        let cert = CertContext::new(buf)?;
        Ok(Certificate(cert))
    }

    pub fn from_pem(buf: &[u8]) -> Result<Certificate, Error> {
        match str::from_utf8(buf) {
            Ok(s) => {
                let cert = CertContext::from_pem(s)?;
                Ok(Certificate(cert))
            }
            Err(_) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "PEM representation contains non-UTF-8 bytes",
            ).into()),
        }
    }

    pub fn to_der(&self) -> Result<Vec<u8>, Error> {
        Ok(self.0.to_der().to_vec())
    }
}

pub struct MidHandshakeTlsStream<S>(tls_stream::MidHandshakeTlsStream<S>);

impl<S> fmt::Debug for MidHandshakeTlsStream<S>
where
    S: fmt::Debug,
{
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&self.0, fmt)
    }
}

impl<S> MidHandshakeTlsStream<S> {
    pub fn get_ref(&self) -> &S {
        self.0.get_ref()
    }

    pub fn get_mut(&mut self) -> &mut S {
        self.0.get_mut()
    }
}

impl<S> MidHandshakeTlsStream<S>
where
    S: io::Read + io::Write,
{
    pub fn handshake(self) -> Result<TlsStream<S>, HandshakeError<S>> {
        match self.0.handshake() {
            Ok(s) => Ok(TlsStream(s)),
            Err(e) => Err(e.into()),
        }
    }
}

pub enum HandshakeError<S> {
    Failure(Error),
    WouldBlock(MidHandshakeTlsStream<S>),
}

impl<S> From<tls_stream::HandshakeError<S>> for HandshakeError<S> {
    fn from(e: tls_stream::HandshakeError<S>) -> HandshakeError<S> {
        match e {
            tls_stream::HandshakeError::Failure(e) => HandshakeError::Failure(e.into()),
            tls_stream::HandshakeError::Interrupted(s) => {
                HandshakeError::WouldBlock(MidHandshakeTlsStream(s))
            }
        }
    }
}

impl<S> From<io::Error> for HandshakeError<S> {
    fn from(e: io::Error) -> HandshakeError<S> {
        HandshakeError::Failure(e.into())
    }
}

#[derive(Clone)]
pub struct TlsConnector {
    cert: Option<CertContext>,
    roots: CertStore,
    min_protocol: Option<::Protocol>,
    max_protocol: Option<::Protocol>,
    use_sni: bool,
    session_tickets_enabled: bool,
    credentials_cache: Arc<Mutex<VecDeque<CacheEntry>>>,
    accept_invalid_hostnames: bool,
    accept_invalid_certs: bool,
    disable_built_in_roots: bool,
    alpn: Vec<Vec<u8>>,
    supported_algorithms: Vec<Algorithm>,
}

impl TlsConnector {
    pub fn new(builder: &TlsConnectorBuilder) -> Result<TlsConnector, Error> {
        let cert = builder.identity.as_ref().map(|i| i.0.cert.clone());
        let mut roots = Memory::new()?.into_store();
        for cert in &builder.root_certificates {
            roots.add_cert(&(cert.0).0, CertAdd::ReplaceExisting)?;
        }

        Ok(TlsConnector {
            cert,
            roots,
            min_protocol: builder.min_protocol,
            max_protocol: builder.max_protocol,
            use_sni: builder.use_sni,
            session_tickets_enabled: builder.session_tickets_enabled,
            credentials_cache: Arc::new(Mutex::new(VecDeque::with_capacity(CREDENTIAL_CACHE_SIZE))),
            accept_invalid_hostnames: builder.accept_invalid_hostnames,
            accept_invalid_certs: builder.accept_invalid_certs,
            disable_built_in_roots: builder.disable_built_in_roots,
            alpn: builder.alpn.clone(),
            supported_algorithms: match &builder.cipher_suites {
                Some(cipher_suites) => expand_algorithms(cipher_suites),
                None => vec![],
            },
        })
    }

    pub fn connect<S>(&self, domain: &str, stream: S) -> Result<TlsStream<S>, HandshakeError<S>>
    where
        S: io::Read + io::Write,
    {
        let cred = self.get_credentials(domain)?;
        let mut builder = tls_stream::Builder::new();
        builder
            .cert_store(self.roots.clone())
            .domain(domain)
            .use_sni(self.use_sni)
            .accept_invalid_hostnames(self.accept_invalid_hostnames);
        if self.accept_invalid_certs {
            builder.verify_callback(|_| Ok(()));
        } else if self.disable_built_in_roots {
            let roots_copy = self.roots.clone();
            builder.verify_callback(move |res| {
                if let Err(err) = res.result() {
                    // Propagate previous error encountered during normal cert validation.
                    return Err(err);
                }

                if let Some(chain) = res.chain() {
                    if chain
                        .certificates()
                        .any(|cert| roots_copy.certs().any(|root_cert| root_cert == cert))
                    {
                        return Ok(());
                    }
                }

                Err(io::Error::new(
                    io::ErrorKind::Other,
                    "unable to find any user-specified roots in the final cert chain",
                ))
            });
        }
        if !self.alpn.is_empty() {
            builder.request_application_protocols(&self.alpn.iter().map(AsRef::as_ref).collect::<Vec<_>>());
        }
        match builder.connect(cred.clone(), stream) {
            Ok(s) => {
                self.store_credentials(domain, cred);
                Ok(TlsStream(s))
            }
            Err(e) => Err(e.into()),
        }
    }

    fn get_credentials(&self, domain: &str) -> io::Result<SchannelCred> {
        if self.session_tickets_enabled {
            let mut found = None;
            let mut cache = self.credentials_cache.lock().unwrap();
            for i in 0..cache.len() {
                if &cache[i].domain == domain {
                    found = Some(i);
                    break;
                }
            }

            if let Some(idx) = found {
                let now = SystemTime::now();
                let mut entry = cache.remove(idx).unwrap();

                if entry.expiry > now {
                    let ret = entry.credentials.clone();
                    entry.expiry = now + CREDENTIAL_TTL;
                    cache.push_back(entry);
                    return Ok(ret);
                }
            }
        }

        let mut builder = SchannelCred::builder();
        builder.enabled_protocols(convert_protocols(self.min_protocol, self.max_protocol));
        if let Some(cert) = self.cert.as_ref() {
            builder.cert(cert.clone());
        }
        if !self.supported_algorithms.is_empty() {
            builder.supported_algorithms(&self.supported_algorithms);
        }
        builder.acquire(Direction::Outbound)
    }

    fn store_credentials(&self, domain: &str, cred: SchannelCred) {
        if self.session_tickets_enabled {
            let mut found = None;
            let mut cache = self.credentials_cache.lock().unwrap();
            for i in 0..cache.len() {
                if &cache[i].domain == domain {
                    found = Some(i);
                    break;
                }
            }

            if let Some(idx) = found {
                cache.remove(idx).unwrap();
            }

            if cache.len() == CREDENTIAL_CACHE_SIZE {
                cache.pop_front();
            }

            cache.push_back(CacheEntry {
                domain: domain.to_owned(),
                expiry: SystemTime::now() + CREDENTIAL_TTL,
                credentials: cred,
            });
        }
    }
}

#[derive(Clone)]
pub struct TlsAcceptor {
    cert: CertContext,
    min_protocol: Option<::Protocol>,
    max_protocol: Option<::Protocol>,
}

impl TlsAcceptor {
    pub fn new(builder: &TlsAcceptorBuilder) -> Result<TlsAcceptor, Error> {
        Ok(TlsAcceptor {
            cert: builder.identity.0.cert.clone(),
            min_protocol: builder.min_protocol,
            max_protocol: builder.max_protocol,
        })
    }

    pub fn accept<S>(&self, stream: S) -> Result<TlsStream<S>, HandshakeError<S>>
    where
        S: io::Read + io::Write,
    {
        let mut builder = SchannelCred::builder();
        builder.enabled_protocols(convert_protocols(self.min_protocol, self.max_protocol));
        builder.cert(self.cert.clone());
        // FIXME we're probably missing the certificate chain?
        let cred = builder.acquire(Direction::Inbound)?;
        match tls_stream::Builder::new().accept(cred, stream) {
            Ok(s) => Ok(TlsStream(s)),
            Err(e) => Err(e.into()),
        }
    }
}

pub struct TlsStream<S>(tls_stream::TlsStream<S>);

impl<S: fmt::Debug> fmt::Debug for TlsStream<S> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&self.0, fmt)
    }
}

impl<S> TlsStream<S> {
    pub fn get_ref(&self) -> &S {
        self.0.get_ref()
    }

    pub fn get_mut(&mut self) -> &mut S {
        self.0.get_mut()
    }
}

impl<S: io::Read + io::Write> TlsStream<S> {
    pub fn buffered_read_size(&self) -> Result<usize, Error> {
        Ok(self.0.get_buf().len())
    }

    pub fn peer_certificate(&self) -> Result<Option<Certificate>, Error> {
        match self.0.peer_certificate() {
            Ok(cert) => Ok(Some(Certificate(cert))),
            Err(ref e) if e.raw_os_error() == Some(SEC_E_NO_CREDENTIALS as i32) => Ok(None),
            Err(e) => Err(Error(e)),
        }
    }

    pub fn negotiated_alpn(&self) -> Result<Option<Vec<u8>>, Error> {
        Ok(self.0.negotiated_application_protocol()?)
    }

    pub fn tls_server_end_point(&self) -> Result<Option<Vec<u8>>, Error> {
        let cert = if self.0.is_server() {
            self.0.certificate()
        } else {
            self.0.peer_certificate()
        };

        let cert = match cert {
            Ok(cert) => cert,
            Err(ref e) if e.raw_os_error() == Some(SEC_E_NO_CREDENTIALS as i32) => return Ok(None),
            Err(e) => return Err(Error(e)),
        };

        let signature_algorithms = cert.sign_hash_algorithms()?;
        let hash = match signature_algorithms.rsplit('/').next().unwrap() {
            "MD5" | "SHA1" | "SHA256" => HashAlgorithm::sha256(),
            "SHA384" => HashAlgorithm::sha384(),
            "SHA512" => HashAlgorithm::sha512(),
            _ => return Ok(None),
        };

        let digest = cert.fingerprint(hash)?;
        Ok(Some(digest))
    }

    pub fn shutdown(&mut self) -> io::Result<()> {
        self.0.shutdown()?;
        Ok(())
    }
}

impl<S: io::Read + io::Write> io::Read for TlsStream<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl<S: io::Read + io::Write> io::Write for TlsStream<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpStream;

    use crate::TlsConnector;

    fn connect_and_assert(tls: &TlsConnector, domain: &str, port: u16, should_resume: bool) {
        let s = TcpStream::connect((domain, port)).unwrap();
        let stream = tls.connect(domain, s).unwrap();

        assert_eq!((stream.0).0.session_resumed().unwrap(), should_resume);
    }

    #[test]
    fn connect_no_session_ticket_resumption() {
        let tls = TlsConnector::new().unwrap();
        connect_and_assert(&tls, "google.com", 443, false);
        connect_and_assert(&tls, "google.com", 443, false);
    }

    /// Expected to fail on Windows versions where RFC 5077 was not implemented (should just be
    /// Windows 7 and below).
    #[test]
    fn connect_session_ticket_resumption() {
        let mut builder = TlsConnector::builder();
        builder.session_tickets_enabled(true);
        let tls = builder.build().unwrap();

        connect_and_assert(&tls, "google.com", 443, false);
        connect_and_assert(&tls, "google.com", 443, true);
    }

    /// Expected to fail on Windows versions where RFC 5077 was not implemented (should just be
    /// Windows 7 and below).
    #[test]
    fn connect_session_ticket_resumption_two_sites() {
        let mut builder = TlsConnector::builder();
        builder.session_tickets_enabled(true);
        let tls = builder.build().unwrap();

        connect_and_assert(&tls, "google.com", 443, false);
        connect_and_assert(&tls, "mozilla.org", 443, false);
        connect_and_assert(&tls, "google.com", 443, true);
        connect_and_assert(&tls, "mozilla.org", 443, true);
    }
}
