use crate::config::Config;
use crate::state::{AVState, format_time};

pub fn device_description(config: &Config, base_url: &str) -> String {
    let clean_base = if base_url.ends_with('/') {
        base_url.to_string()
    } else {
        format!("{}/", base_url)
    };
    format!(
        r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <URLBase>{base}</URLBase>
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
    <friendlyName>{friendly}</friendlyName>
    <manufacturer>playpnp</manufacturer>
    <manufacturerURL>https://github.com/playpnp/playpnp</manufacturerURL>
    <modelDescription>DLNA MediaRenderer for Windows</modelDescription>
    <modelName>playpnp</modelName>
    <modelNumber>1</modelNumber>
    <modelURL>https://github.com/playpnp/playpnp</modelURL>
    <serialNumber>1</serialNumber>
    <UDN>{udn}</UDN>
    <presentationURL>{base}</presentationURL>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:AVTransport</serviceId>
        <SCPDURL>/AVTransport/scpd.xml</SCPDURL>
        <controlURL>/AVTransport/control</controlURL>
        <eventSubURL>/AVTransport/event</eventSubURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:RenderingControl</serviceId>
        <SCPDURL>/RenderingControl/scpd.xml</SCPDURL>
        <controlURL>/RenderingControl/control</controlURL>
        <eventSubURL>/RenderingControl/event</eventSubURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:ConnectionManager</serviceId>
        <SCPDURL>/ConnectionManager/scpd.xml</SCPDURL>
        <controlURL>/ConnectionManager/control</controlURL>
        <eventSubURL>/ConnectionManager/event</eventSubURL>
      </service>
    </serviceList>
  </device>
</root>"#,
        base = clean_base,
        friendly = escape_xml(&config.friendly_name),
        udn = config.udn(),
    )
}

pub fn avtransport_scpd() -> &'static str {
    include_str!("../resources/scpd/AVTransport.xml")
}

pub fn rendering_control_scpd() -> &'static str {
    include_str!("../resources/scpd/RenderingControl.xml")
}

pub fn connection_manager_scpd() -> &'static str {
    include_str!("../resources/scpd/ConnectionManager.xml")
}

pub fn soap_envelope(body: &str) -> String {
    format!(
        r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body>{}</s:Body></s:Envelope>"#,
        body
    )
}

pub fn soap_fault(code: u32, description: &str) -> String {
    let body = format!(
        r#"<s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0"><errorCode>{}</errorCode><errorDescription>{}</errorDescription></UPnPError></detail></s:Fault>"#,
        code,
        escape_xml(description)
    );
    soap_envelope(&body)
}

pub fn soap_response(action: &str, service: &str, inner: &str) -> String {
    let body = format!(r#"<u:{action}Response xmlns:u="{service}">{inner}</u:{action}Response>"#,);
    soap_envelope(&body)
}

// LastChange for AVTransport eventing
pub fn avtransport_last_change(state: &AVState) -> String {
    // This is the XML that goes inside <LastChange> escaped
    let duration = if state.duration.as_secs() == 0 {
        "00:00:00".to_string()
    } else {
        format_time(state.duration)
    };
    let rel_time = format_time(state.position);
    let abs_time = rel_time.clone();
    // Minimal Evented state vars per AV spec
    let inner = format!(
        r#"<Event xmlns="urn:schemas-upnp-org:metadata-1-0/AVT/"><InstanceID val="0"><TransportState val="{ts}"/><CurrentTrackURI val="{uri}"/><CurrentTrackDuration val="{dur}"/><RelativeTimePosition val="{rel}"/><AbsoluteTimePosition val="{abs}"/><CurrentTransportActions val="Play,Stop,Pause,Seek,Next,Previous"/><TransportStatus val="OK"/></InstanceID></Event>"#,
        ts = state.transport_state.as_str(),
        uri = escape_xml(&state.current_uri),
        dur = duration,
        rel = rel_time,
        abs = abs_time,
    );
    escape_xml(&inner)
}

pub fn rendering_last_change(state: &AVState) -> String {
    let inner = format!(
        r#"<Event xmlns="urn:schemas-upnp-org:metadata-1-0/RCS/"><InstanceID val="0"><Volume channel="Master" val="{}"/><Mute channel="Master" val="{}"/></InstanceID></Event>"#,
        state.volume,
        if state.mute { "1" } else { "0" }
    );
    escape_xml(&inner)
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

// Extract SOAP action from header "urn:schemas-upnp-org:service:AVTransport:1#Play"
pub fn parse_soap_action(header: &str) -> Option<String> {
    let h = header.trim().trim_matches('"').trim_matches('\'');
    // action is after '#'
    if let Some(pos) = h.rfind('#') {
        Some(h[pos + 1..].to_string())
    } else if let Some(pos) = h.rfind('/') {
        Some(h[pos + 1..].to_string())
    } else {
        Some(h.to_string())
    }
}

pub fn extract_tag(body: &str, tag: &str) -> Option<String> {
    // naive case-sensitive search for <Tag> or <u:Tag> or <Tag ...>
    // try multiple patterns
    for prefix in ["<", "<u:", "<s:"] {
        let open = format!("{}{}", prefix, tag);
        if let Some(start) = body.find(&open) {
            // find '>' after open
            let after_open = &body[start..];
            if let Some(gt) = after_open.find('>') {
                let content_start = start + gt + 1;
                let close1 = format!("</{}>", tag);
                let close2 = format!("</u:{}>", tag);
                let close3 = format!("</s:{}>", tag);
                for close in [close1, close2, close3] {
                    if let Some(end_offset) = body[content_start..].find(&close) {
                        let content = &body[content_start..content_start + end_offset];
                        return Some(content.trim().to_string());
                    }
                }
            }
        }
    }
    // fallback simple
    let open_simple = format!("<{}>", tag);
    let close_simple = format!("</{}>", tag);
    if let Some(s) = body.find(&open_simple) {
        if let Some(e) = body[s..].find(&close_simple) {
            let cs = s + open_simple.len();
            let ce = s + e;
            return Some(body[cs..ce].trim().to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_parse_soap_action() {
        assert_eq!(
            parse_soap_action("\"urn:schemas-upnp-org:service:AVTransport:1#Play\""),
            Some("Play".into())
        );
        assert_eq!(
            parse_soap_action("urn:schemas-upnp-org:service:AVTransport:1#SetAVTransportURI"),
            Some("SetAVTransportURI".into())
        );
    }
    #[test]
    fn test_extract_tag() {
        let body = r#"<s:Envelope><s:Body><u:SetAVTransportURI xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><InstanceID>0</InstanceID><CurrentURI>http://example.com/video.mp4</CurrentURI></u:SetAVTransportURI></s:Body></s:Envelope>"#;
        assert_eq!(extract_tag(body, "InstanceID"), Some("0".into()));
        assert_eq!(
            extract_tag(body, "CurrentURI"),
            Some("http://example.com/video.mp4".into())
        );
    }
}
