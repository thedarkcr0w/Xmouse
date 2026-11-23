//! Xbox home streaming ("xHome") REST API: consoles, sessions, SDP/ICE exchange, power.

use std::net::Ipv6Addr;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ConsoleInfo {
    pub server_id: String,
    pub name: String,
    pub console_type: String,
    pub power_state: String,
    pub out_of_home: bool,
}

impl ConsoleInfo {
    pub fn type_label(&self) -> &str {
        match self.console_type.as_str() {
            "XboxSeriesX" => "Xbox Series X",
            "XboxSeriesS" => "Xbox Series S",
            "XboxOne" => "Xbox One",
            "XboxOneS" => "Xbox One S",
            "XboxOneX" => "Xbox One X",
            other => other,
        }
    }
    pub fn power_label(&self) -> &str {
        match self.power_state.as_str() {
            "On" => "On",
            "ConnectedStandby" => "Asleep",
            "Off" => "Off",
            other => other,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IceCandidate {
    pub candidate: String,
    #[serde(default, rename = "sdpMid")]
    pub sdp_mid: Option<Value>,
    #[serde(default, rename = "sdpMLineIndex")]
    pub sdp_mline_index: Option<Value>,
}

// We speak the browser (xbox.com) protocol, so identify as the browser client everywhere
// (a "native" device info with sdkType "web" is a mixed signal to the console).
const DEVICE_INFO: &str = r#"{"appInfo":{"env":{"clientAppId":"www.xbox.com","clientAppType":"browser","clientAppVersion":"21.1.98","clientSdkVersion":"8.5.3","httpEnvironment":"prod","sdkInstallId":""}},"dev":{"hw":{"make":"Microsoft","model":"unknown","sdktype":"web"},"os":{"name":"windows","ver":"22631.2715","platform":"desktop"},"displayInfo":{"dimensions":{"widthInPixels":1920,"heightInPixels":1080},"pixelDensity":{"dpiX":2,"dpiY":2}},"browser":{"browserName":"chrome","browserVersion":"119.0"}}}"#;

#[derive(Clone)]
pub struct XHome {
    base: String,
    token: String,
    http: reqwest::Client,
}

impl XHome {
    pub fn new(base: &str, token: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()
                .expect("http client"),
        }
    }

    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
        extra: &[(&str, &str)],
        retry_204: bool,
    ) -> Result<Value> {
        for _ in 0..80 {
            let mut req = self
                .http
                .request(method.clone(), format!("{}{}", self.base, path))
                .bearer_auth(&self.token)
                .header("Content-Type", "application/json")
                .header("Accept", "application/json")
                .header("X-Gssv-Client", "XboxComBrowser")
                .header("X-MS-Device-Info", DEVICE_INFO);
            for (k, v) in extra {
                req = req.header(*k, *v);
            }
            if let Some(b) = &body {
                req = req.body(b.to_string());
            }
            let res = req
                .send()
                .await
                .with_context(|| format!("Request to {path} failed"))?;
            let status = res.status().as_u16();
            if status == 204 && retry_204 {
                tokio::time::sleep(Duration::from_millis(750)).await;
                continue;
            }
            let text = res.text().await.unwrap_or_default();
            if (200..300).contains(&status) {
                return Ok(serde_json::from_str(&text).unwrap_or(Value::Null));
            }
            return Err(anyhow!(HttpError {
                status,
                body: text.chars().take(400).collect()
            }));
        }
        bail!("Timed out waiting for the Xbox service ({path})")
    }

    async fn get(&self, path: &str) -> Result<Value> {
        self.request(reqwest::Method::GET, path, None, &[], true)
            .await
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.request(reqwest::Method::POST, path, Some(body), &[], false)
            .await
    }

    pub async fn consoles(&self) -> Result<Vec<ConsoleInfo>> {
        let v = self
            .request(reqwest::Method::GET, "/v6/servers/home", None, &[], false)
            .await?;
        Ok(v["results"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|c| ConsoleInfo {
                        server_id: c["serverId"].as_str().unwrap_or_default().into(),
                        name: c["deviceName"].as_str().unwrap_or("Xbox").into(),
                        console_type: c["consoleType"].as_str().unwrap_or_default().into(),
                        power_state: c["powerState"].as_str().unwrap_or("Unknown").into(),
                        out_of_home: c["outOfHomeWarning"].as_bool().unwrap_or(false),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Ask the service to start remote play on a console. Returns the session id.
    pub async fn start(&self, server_id: &str, locale: &str) -> Result<String> {
        let tz = chrono_offset_minutes();
        let v = self
            .request(
                reqwest::Method::POST,
                "/v5/sessions/home/play",
                Some(json!({
                    "titleId": "", "systemUpdateGroup": "", "clientSessionId": "",
                    "settings": {
                        "nanoVersion": "V3;WebrtcTransport.dll", "enableOptionalDataCollection": false,
                        "enableTextToSpeech": false, "highContrast": 0,
                        "locale": locale, "useIceConnection": false, "timezoneOffsetMinutes": tz,
                        "sdkType": "web", "osName": "windows"
                    },
                    "serverId": server_id, "fallbackRegionNames": []
                })),
                &[],
                false,
            )
            .await?;
        let path = v["sessionPath"]
            .as_str()
            .context("No sessionPath in response")?;
        path.split('/')
            .nth(3)
            .map(|s| s.to_string())
            .context("Unexpected sessionPath")
    }

    pub async fn state(&self, id: &str) -> Result<Value> {
        self.get(&format!("/v5/sessions/home/{id}/state")).await
    }

    pub async fn connect(&self, id: &str, user_token: &str) -> Result<()> {
        self.post(
            &format!("/v5/sessions/home/{id}/connect"),
            json!({ "userToken": user_token }),
        )
        .await?;
        Ok(())
    }

    pub async fn keepalive(&self, id: &str) -> Result<()> {
        self.post(&format!("/v5/sessions/home/{id}/keepalive"), json!({}))
            .await?;
        Ok(())
    }

    pub async fn stop(&self, id: &str) -> Result<()> {
        self.request(
            reqwest::Method::DELETE,
            &format!("/v5/sessions/home/{id}"),
            None,
            &[],
            false,
        )
        .await?;
        Ok(())
    }

    /// Send our SDP offer, get the console's answer SDP.
    pub async fn exchange_sdp(&self, id: &str, sdp: &str) -> Result<String> {
        self.post(
            &format!("/v5/sessions/home/{id}/sdp"),
            json!({
                "messageType": "offer", "sdp": sdp, "requestId": "1",
                "configuration": {
                    "chatConfiguration": {
                        "bytesPerSample": 2, "expectedClipDurationMs": 20,
                        "format": { "codec": "opus", "container": "webm" }, "numChannels": 1, "sampleFrequencyHz": 24000
                    },
                    "chat": { "minVersion": 1, "maxVersion": 1 },
                    "control": { "minVersion": 1, "maxVersion": 3 },
                    "input": { "minVersion": 1, "maxVersion": 9 },
                    "message": { "minVersion": 1, "maxVersion": 1 },
                    "reliableinput": { "minVersion": 9, "maxVersion": 9 },
                    "unreliableinput": { "minVersion": 9, "maxVersion": 9 }
                }
            }),
        )
        .await?;
        let v = self.get(&format!("/v5/sessions/home/{id}/sdp")).await?;
        let exchange: Value =
            serde_json::from_str(v["exchangeResponse"].as_str().context("No SDP answer")?)?;
        if let Some(sdp) = exchange["sdp"].as_str() {
            return Ok(sdp.to_string());
        }
        bail!("The console rejected the connection offer: {}", exchange)
    }

    /// Send our ICE candidates, get the console's.
    pub async fn exchange_ice(
        &self,
        id: &str,
        candidates: &[IceCandidate],
    ) -> Result<Vec<IceCandidate>> {
        // Same shape as the browser client: an array of JSON-encoded candidate strings.
        let encoded: Vec<String> = candidates
            .iter()
            .map(|c| {
                json!({
                    "candidate": c.candidate,
                    "sdpMid": c.sdp_mid.as_ref().and_then(|v| v.as_str().map(String::from)).unwrap_or_else(|| "0".into()),
                    "sdpMLineIndex": c.sdp_mline_index.as_ref().and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))).unwrap_or(0),
                })
                .to_string()
            })
            .collect();
        let posted = self
            .post(
                &format!("/v5/sessions/home/{id}/ice"),
                json!({ "candidates": encoded }),
            )
            .await?;
        log::info!(
            "ICE: posted {} local candidates, service said {posted}",
            encoded.len()
        );
        let v = self.get(&format!("/v5/sessions/home/{id}/ice")).await?;
        log::info!("ICE: console candidates {v}");
        let remote: Vec<IceCandidate> =
            serde_json::from_str(v["exchangeResponse"].as_str().context("No ICE answer")?)?;
        Ok(expand_teredo(remote))
    }
}

#[derive(Debug)]
pub struct HttpError {
    pub status: u16,
    pub body: String,
}
impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP {} {}", self.status, self.body)
    }
}
impl std::error::Error for HttpError {}

fn chrono_offset_minutes() -> i64 {
    // Without a timezone crate: the service only uses this for display; UTC is fine.
    0
}

/// Consoles often advertise a Teredo IPv6 address; also offer the IPv4 address embedded in it.
///
/// Only when that's all there is: newer console software lists its real IPv4 addresses (LAN
/// host and public srflx) itself, and the guessed `<public ip>:<teredo port>` candidates then
/// only add dead pairs whose ICMP errors used to break the connection on Windows.
pub fn expand_teredo(remote: Vec<IceCandidate>) -> Vec<IceCandidate> {
    let has_ipv4 = remote.iter().any(|c| {
        c.candidate
            .split_whitespace()
            .nth(4)
            .is_some_and(|ip| ip.parse::<std::net::Ipv4Addr>().is_ok())
    });
    if has_ipv4 {
        return remote;
    }
    let mut out = Vec::new();
    for c in remote {
        let parts: Vec<&str> = c.candidate.split_whitespace().collect();
        if parts.len() > 4 {
            if let Some((ip, port)) = teredo_client(parts[4]) {
                for (foundation, p) in [(10, 9002u16), (11, port)] {
                    out.push(IceCandidate {
                        candidate: format!("a=candidate:{foundation} 1 UDP 1 {ip} {p} typ host "),
                        sdp_mid: Some(json!("0")),
                        sdp_mline_index: Some(json!("0")),
                    });
                }
            }
        }
        out.push(c);
    }
    out
}

/// Decode the client IPv4 address and port embedded in a Teredo (2001:0::/32) address.
pub fn teredo_client(addr: &str) -> Option<(String, u16)> {
    let ip: Ipv6Addr = addr.split('%').next()?.parse().ok()?;
    let s = ip.segments();
    if s[0] != 0x2001 || s[1] != 0 {
        return None;
    }
    let port = !s[5];
    let hi = !s[6];
    let lo = !s[7];
    Some((
        format!("{}.{}.{}.{}", hi >> 8, hi & 0xff, lo >> 8, lo & 0xff),
        port,
    ))
}

/// Wake a console from Sleep via the Xbox remote management service.
pub async fn wake_console(web_token: &str, uhs: &str, console_id: &str) -> Result<()> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;
    let res = http
        .post("https://xccs.xboxlive.com/commands")
        .header("Authorization", format!("XBL3.0 x={uhs};{web_token}"))
        .header("x-xbl-contract-version", "4")
        .header("skillplatform", "RemoteManagement")
        .json(&json!({
            "destination": "Xbox", "type": "Power", "command": "WakeUp",
            "sessionId": uuid::Uuid::new_v4().to_string(), "sourceId": "com.microsoft.smartglass",
            "parameters": [], "linkedXboxId": console_id
        }))
        .send()
        .await?;
    if !res.status().is_success() {
        bail!("Wake-up failed (HTTP {})", res.status().as_u16());
    }
    Ok(())
}

/// Explain a session failure in plain words.
pub fn describe_failure(details: &Value) -> String {
    let code = details["code"].as_str().unwrap_or("Unknown");
    let message = details["message"].as_str().unwrap_or("");
    if code == "WNSError" && message.contains("WaitingForServerToRegister") {
        return "Your Xbox is not reachable. Make sure it is in Sleep power mode, \"Remote features\" are enabled, and it is signed in with this account.".into();
    }
    format!("[{code}] {message}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn teredo_decoding() {
        // RFC 4380 example: server 65.54.227.120, client 192.0.2.45:40000
        let (ip, port) = teredo_client("2001:0000:4136:e378:8000:63bf:3fff:fdd2").unwrap();
        assert_eq!(ip, "192.0.2.45");
        assert_eq!(port, 40000);
        assert!(teredo_client("fe80::1").is_none());
    }

    #[test]
    fn expands_teredo_candidates() {
        let c = IceCandidate {
            candidate:
                "a=candidate:1 1 UDP 100 2001:0:4136:e378:8000:63bf:3fff:fdd2 9002 typ host ".into(),
            sdp_mid: Some(json!("0")),
            sdp_mline_index: Some(json!("0")),
        };
        let out = expand_teredo(vec![c.clone()]);
        assert_eq!(out.len(), 3);
        assert!(out[0].candidate.contains("192.0.2.45 9002"));
        assert!(out[1].candidate.contains("192.0.2.45 40000"));
        assert_eq!(out[2], c);
    }

    #[test]
    fn no_teredo_guesses_when_the_console_lists_ipv4() {
        let teredo = IceCandidate {
            candidate:
                "a=candidate:2 1 UDP 1 2001:0:4136:e378:8000:63bf:3fff:fdd2 9002 typ host ".into(),
            sdp_mid: Some(json!("0")),
            sdp_mline_index: Some(json!("0")),
        };
        let lan = IceCandidate {
            candidate: "candidate:1 1 udp 2122194687 192.168.0.61 9002 typ host generation 0"
                .into(),
            sdp_mid: Some(json!("0")),
            sdp_mline_index: Some(json!(0)),
        };
        let out = expand_teredo(vec![lan.clone(), teredo.clone()]);
        assert_eq!(out, vec![lan, teredo]);
    }
}
