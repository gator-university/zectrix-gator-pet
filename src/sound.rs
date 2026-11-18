use crate::model::{Status, Task};

/// An actual transition, never a heartbeat or a replayed historical record.
pub fn transition(before: Option<Status>, after: Status) -> bool {
    before != Some(after) && matches!(after, Status::Waiting | Status::Done)
}

/// 是否该为这个事件响铃。`allow_chime=false` 用于首次扫描：那次读到的多半是
/// 历史状态的回放，逐个响会把旧事件当新通知。首扫之后由调用方对「当前正停在
/// 等你处理」补一次提醒——设备超时重连后等待页会无声出现，那才是真漏报。
pub fn should_chime(allow_chime: bool, before: Option<Status>, after: Status) -> bool {
    allow_chime && transition(before, after)
}

/// 「已提醒过」的指纹，用来保证同一次等待/完成只响一次——包括跨进程重启、
/// 设备超时重连之后，这样「补提醒」不会在每次重连时重复吵人。
///
/// 只存 `sha256(<任务 key>|<状态词>)` 的前 16 位十六进制，**不含标题、提问或
/// 确认文案**：这些内容可能包含提示词正文，不应该出现在 state.json 里。同一个
/// 任务同一种状态只提醒一次；换任务才重新提醒。
pub fn notice_sig(task: &Task) -> Option<String> {
    let state = match task.status() {
        Status::Waiting => "waiting",
        Status::Done => "done",
        _ => return None,
    };
    let digest = sha256_hex(format!("{}|{}", task.key(), state).as_bytes());
    Some(digest[..16].to_string())
}

/// 旧 state.json 的 `announced` 存的是「key|w|文案」，最长可含 60 字提问。
/// 只保留新格式（16 位小写十六进制），其余在加载时清掉。
pub fn is_notice_hash(value: &str) -> bool {
    value.len() == 16
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// 本地 SHA-256：项目不能为一个 16 位指纹新增依赖（Cargo.toml 也不在本次
/// 改动范围内），实现只服务于 `notice_sig`，并由已知向量测试兜底。
fn sha256_hex(input: &[u8]) -> String {
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut message = input.to_vec();
    let bit_len = (message.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in message.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            let at = i * 4;
            *word = u32::from_be_bytes([chunk[at], chunk[at + 1], chunk[at + 2], chunk[at + 3]]);
        }
        for i in 16..64 {
            let s0 =
                w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for (i, k) in SHA256_K.iter().enumerate() {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(*k)
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }

    state.iter().map(|word| format!("{word:08x}")).collect()
}
pub fn still_relevant(task: &Task, state: Status) -> bool {
    !task.stale && task.status() == state
}
/// 设备固件对单个播放流的接收队列只有 6 帧（360 ms）；实测超过 5 帧即返回
/// 「处理失败」（RESULT 结束原因 7），与音量、发送节奏无关。因此单条提示音
/// 必须压在 300 ms 以内 —— 用「短促双音」而不是长旋律来表达状态。
pub const MAX_TONE_MS: usize = 300;

/// Locally synthesized 16 kHz mono PCM16. Short envelopes prevent clicks.
pub fn pcm(state: Status) -> Vec<u8> {
    let notes: &[(f32, usize)] = match state {
        // 两声短「嘀」：需要你回来处理
        Status::Waiting => &[(880.0, 80), (0.0, 40), (880.0, 80)],
        // 上行三连音：任务完成
        Status::Done => &[(659.25, 60), (783.99, 60), (1046.5, 90)],
        // 下行两音：出错或中途中止
        _ => &[(440.0, 90), (349.23, 90)],
    };
    let mut bytes = Vec::new();
    for &(frequency, milliseconds) in notes {
        let count = milliseconds * 16;
        for i in 0..count {
            let attack = (i as f32 / 160.0).min(1.0);
            let release = ((count - i) as f32 / 480.0).min(1.0);
            let phase = std::f32::consts::TAU * frequency * i as f32 / 16000.0;
            let sample =
                ((phase.sin() + 0.15 * (2.0 * phase).sin()) * attack * release * 14000.0) as i16;
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
    }
    bytes
}
pub fn wav(state: Status) -> Vec<u8> {
    let pcm = pcm(state);
    let length = pcm.len() as u32;
    let mut bytes = Vec::with_capacity(pcm.len() + 44);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(length + 36).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&16000u32.to_le_bytes());
    bytes.extend_from_slice(&32000u32.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(&pcm);
    bytes
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chimes_only_on_transition_and_are_short_distinct_pcm() {
        assert!(transition(Some(Status::Working), Status::Waiting));
        assert!(transition(Some(Status::Working), Status::Done));
        assert!(!transition(Some(Status::Waiting), Status::Waiting));
        assert!(!transition(Some(Status::Done), Status::Done));
        assert!(!transition(Some(Status::Waiting), Status::Working));
        // 首扫一律静默（历史回放），之后的真实变化才响。
        assert!(!should_chime(false, Some(Status::Working), Status::Waiting));
        assert!(!should_chime(false, None, Status::Done));
        assert!(should_chime(true, Some(Status::Working), Status::Waiting));
        assert!(should_chime(true, Some(Status::Working), Status::Done));
        assert!(!should_chime(true, Some(Status::Waiting), Status::Waiting));
        assert_ne!(pcm(Status::Waiting), pcm(Status::Done));
        for status in [Status::Waiting, Status::Done, Status::Error] {
            let bytes = pcm(status);
            // 设备单流上限：300 ms = 4800 采样 = 9600 字节；短于 60 ms 会被当成异常帧
            assert!(
                bytes.len() <= MAX_TONE_MS * 32,
                "{status:?} 提示音超过设备单流上限：{} 字节",
                bytes.len()
            );
            assert!(bytes.len() >= 1920, "{status:?} 提示音过短");
            assert_eq!(bytes.len() % 2, 0);
            let encoded = wav(status);
            assert_eq!(&encoded[..4], b"RIFF");
            assert_eq!(&encoded[8..16], b"WAVEfmt ");
            assert_eq!(&encoded[24..28], &16000u32.to_le_bytes());
            assert_eq!(&encoded[44..], &bytes);
        }
    }

    fn task(id: &str, status: Status, detail: &str, seq: u64) -> Task {
        Task {
            event: crate::model::Event {
                task_id: id.into(),
                thread_id: None,
                seq,
                status,
                title: "整理今天的待办".into(),
                detail: detail.into(),
                progress: None,
                progress_source: crate::model::ProgressSource::Agent,
                progress_steps: None,
                agent: "DeepSeek Harness".into(),
            },
            source: "dsh".into(),
            updated_at: 1_700_000_000,
            stale: false,
            subagent_busy: false,
        }
    }

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn notice_signature_is_hash_of_task_and_status_only() {
        // 同一条等待被重复上报（进度、文案、序号抖动）时指纹不变：不该反复响。
        let waiting = task(
            "t1",
            Status::Waiting,
            "有写入操作等你确认 · 请回电脑点允许",
            5,
        );
        let again = task("t1", Status::Waiting, "换了一张确认卡", 9);
        assert_eq!(notice_sig(&waiting), notice_sig(&again));
        // 换一个任务 → 必须重新提醒。
        assert_ne!(
            notice_sig(&waiting),
            notice_sig(&task("t2", Status::Waiting, "同一个提问", 5))
        );
        // 同一次完成跨重启不重复响；状态词不同则指纹不同。
        assert_eq!(
            notice_sig(&task("t1", Status::Done, "", 7)),
            notice_sig(&task("t1", Status::Done, "", 9))
        );
        assert_ne!(
            notice_sig(&task("t1", Status::Done, "", 7)),
            notice_sig(&task("t1", Status::Waiting, "", 7))
        );
        // 工作中的状态不是提醒点。
        assert_eq!(
            notice_sig(&task("t1", Status::Working, "在跑测试", 7)),
            None
        );
    }

    #[test]
    fn announced_contains_hash_not_text() {
        let task = task("t1", Status::Waiting, "机密：把密码发我", 5);
        let sig = notice_sig(&task).expect("等待态应有指纹");
        assert_eq!(sig.len(), 16, "指纹只保留 sha256 前 16 位");
        assert!(is_notice_hash(&sig), "必须是小写十六进制：{sig}");
        for secret in ["机密", "密码", "t1", "dsh"] {
            assert!(!sig.contains(secret), "指纹泄露了 {secret}：{sig}");
        }
        // 旧格式（key|状态|60 字文案）在加载时会被清掉。
        assert!(!is_notice_hash(
            "dsh:t1|w|要删除 3 个文件，允许吗？"
        ));
        assert!(!is_notice_hash("0123456789abcde"));
        assert!(!is_notice_hash("0123456789ABCDEF"));
    }
}
