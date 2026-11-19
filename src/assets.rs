//! Embedded visual and audio assets shared by desktop playback and the chat UI.
//! 内嵌的图像和音频素材由桌面播放与聊天界面共用。

#[derive(Clone, Copy)]
pub(crate) struct Clip {
    pub(crate) name: &'static str,
    pub(crate) image: &'static [u8],
    pub(crate) animation: &'static [u8],
    pub(crate) sound: &'static [u8],
}

macro_rules! ping {
    ($name:literal, $image:literal, $sound:literal) => {
        Clip {
            name: $name,
            image: include_bytes!(concat!("../assets/textures/", $image, ".png")),
            animation: include_bytes!(concat!("../assets/ping-fx/", $image, ".webp")),
            sound: include_bytes!(concat!("../assets/sounds/", $sound, ".wav")),
        }
    };
}

macro_rules! emote {
    ($name:literal, $asset:literal) => {
        Clip {
            name: $name,
            image: include_bytes!(concat!("../assets/emotes/", $asset, ".png")),
            animation: include_bytes!(concat!("../assets/emotes/", $asset, ".webp")),
            sound: include_bytes!(concat!("../assets/emotes/", $asset, ".wav")),
        }
    };
}

// Array indices are sent over the room protocol and stored in local history.
// 数组下标用于房间协议传输和本地历史存储。
pub(crate) const PINGS: [Clip; 9] = [
    ping!("危险", "pingwheel_retreatrender", "SRP_12"),
    ping!("推进", "pingwheel_pushrender", "SRP_9"),
    ping!("正在赶来", "omw_cyan", "OnMyWay"),
    ping!("全力进攻", "png_all_in", "SRP_4"),
    ping!("请求协助", "png_assist_me_128x128", "ComeHere"),
    ping!("需要视野", "mt11_18_vision", "SRP_7"),
    ping!("敌人消失", "png_enemymissing_128x128", "MIA"),
    ping!("敌方视野", "pingwheel_enemyvisionrender", "AreaIsWarded"),
    ping!("标记", "pingwheel_basicpingrender", "Base"),
];

// Existing indices must keep referring to the same emotes.
// 已有下标必须继续指向同一个表情。
pub(crate) const EMOTES: [Clip; 13] = [
    emote!("愤怒蜜蜂", "bee_mad"),
    emote!("伤心蜜蜂", "bee_sad"),
    emote!("爱心", "gg_heart"),
    emote!("扶额", "facepalm"),
    emote!("赞", "scout_approved"),
    emote!("无语", "unimpressed"),
    emote!("今天不行", "not_today"),
    emote!("魄罗点心", "poro_snax"),
    emote!("不配", "unworthy_cn"),
    emote!("伤心小猫", "sad_kitten"),
    emote!("调皮魄罗", "cheeky_poro"),
    emote!("生气小猫", "angry_kitty"),
    emote!("开心蜜蜂", "bee_happy"),
];

// Wheel slots map to stable catalog IDs rather than changing the wire format.
// 轮盘槽位映射到稳定的素材 ID，不改变网络协议。
pub(crate) const DEFAULT_EMOTE_SLOTS: [usize; 8] = [8, 7, 9, 10, 11, 2, 12, 0];
pub(crate) const EMOTE_HUB: usize = 3;
pub(crate) const WHEEL_TICK: &[u8] = include_bytes!("../assets/sounds/button.wav");
