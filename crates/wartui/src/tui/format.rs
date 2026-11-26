use wartui_proto::plan::{ChannelSet, index_of};

/// A duration as `HH:MM:SS`.
pub(super) fn elapsed(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    format!("{:02}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
}

/// Channel numbers as the set of their scan-table indices, so [`crate::text::channel_list`]
/// words them in runs. A number outside the table has no index and is left out.
pub(super) fn channel_set(channels: &[u8]) -> ChannelSet {
    let mut set = ChannelSet::empty();
    for idx in channels.iter().filter_map(|&channel| index_of(channel)) {
        set.insert(idx);
    }
    set
}
