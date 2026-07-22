//! Linux MPRIS backend. The ignored probe is kept as a reproducible check of
//! the session bus and player property shapes before the worker is enabled.

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "requires a desktop session with an MPRIS player"]
    fn mpris_snapshot_probe() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let connection = zbus::Connection::session().await.expect("session bus");
            let dbus = zbus::fdo::DBusProxy::new(&connection)
                .await
                .expect("DBus proxy");
            let names = dbus.list_names().await.expect("ListNames");
            let name = names
                .iter()
                .find(|name| name.as_str().starts_with("org.mpris.MediaPlayer2."))
                .expect("start an MPRIS player")
                .as_str();
            let proxy = zbus::Proxy::new(
                &connection,
                name,
                "/org/mpris/MediaPlayer2",
                "org.mpris.MediaPlayer2.Player",
            )
            .await
            .expect("MPRIS Player proxy");
            let status: String = proxy
                .get_property("PlaybackStatus")
                .await
                .expect("PlaybackStatus");
            let position: i64 = proxy.get_property("Position").await.expect("Position");
            let metadata: std::collections::HashMap<String, zbus::zvariant::OwnedValue> = proxy
                .get_property("Metadata")
                .await
                .expect("Metadata");
            eprintln!(
                "MPRIS probe: name={name} status={status} position_us={position} metadata_keys={:?}",
                metadata.keys().collect::<Vec<_>>()
            );
            assert!(metadata.contains_key("xesam:title"));
        });
    }
}
