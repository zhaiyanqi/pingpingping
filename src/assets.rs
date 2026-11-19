//! Embedded visual and audio assets shared by desktop playback and the chat UI.
//! 内嵌的图像和音频素材由桌面播放与聊天界面共用。

#[derive(Clone, Copy)]
pub(crate) struct Clip {
    pub(crate) name: &'static str,
    pub(crate) image: &'static [u8],
    pub(crate) animation: Option<&'static [u8]>,
    pub(crate) sound: &'static [u8],
}

macro_rules! ping {
    ($name:literal, $image:literal, $sound:literal) => {
        Clip {
            name: $name,
            image: include_bytes!(concat!("../assets/textures/", $image, ".png")),
            animation: None,
            sound: include_bytes!(concat!("../assets/sounds/", $sound, ".wav")),
        }
    };
}

macro_rules! emote {
    ($name:literal, $asset:literal) => {
        Clip {
            name: $name,
            image: include_bytes!(concat!("../assets/emotes/", $asset, ".png")),
            animation: Some(include_bytes!(concat!(
                "../assets/emotes/",
                $asset,
                ".webp"
            ))),
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
pub(crate) const EMOTES: [Clip; 8] = [
    emote!("愤怒蜜蜂", "bee_mad"),
    emote!("伤心蜜蜂", "bee_sad"),
    emote!("爱心", "gg_heart"),
    emote!("扶额", "facepalm"),
    emote!("赞", "scout_approved"),
    emote!("无语", "unimpressed"),
    emote!("今天不行", "not_today"),
    emote!("魄罗点心", "poro_snax"),
];
