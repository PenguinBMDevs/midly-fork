//! 合成 MIDI 用例：覆盖指导书 P1 要求的 running status、非零释放力度、
//! 同键重触发、同 tick 文件序、0xD0 错位回归、未闭合音符、EOT 边界、
//! SysEx/Escape、文本 meta 全集、多轨不同 EOT、畸形宽松路径等。
//!
//! 每个用例构造原始字节后走 `run_oracle`（Smf 参考链 vs 快路径对拍）。

use super::run_oracle;

// ── 字节构造工具 ──

/// 标准 MIDI VLQ 编码。
fn vlq(mut v: u32) -> Vec<u8> {
    let mut out = vec![(v & 0x7F) as u8];
    v >>= 7;
    while v > 0 {
        out.push(((v & 0x7F) | 0x80) as u8);
        v >>= 7;
    }
    out.reverse();
    out
}

/// delta + 事件体。
fn ev(delta: u32, body: &[u8]) -> Vec<u8> {
    let mut b = vlq(delta);
    b.extend_from_slice(body);
    b
}

/// meta 事件。
fn meta(delta: u32, ty: u8, data: &[u8]) -> Vec<u8> {
    let mut b = ev(delta, &[0xFF, ty]);
    b.extend(vlq(data.len() as u32));
    b.extend_from_slice(data);
    b
}

/// EOT。
fn eot(delta: u32) -> Vec<u8> {
    meta(delta, 0x2F, &[])
}

/// SysEx / Escape（status = 0xF0 / 0xF7）。
fn sysex(delta: u32, status: u8, data: &[u8]) -> Vec<u8> {
    let mut b = ev(delta, &[status]);
    b.extend(vlq(data.len() as u32));
    b.extend_from_slice(data);
    b
}

/// MTrk 块。
fn track(events: &[u8]) -> Vec<u8> {
    let mut b = b"MTrk".to_vec();
    b.extend((events.len() as u32).to_be_bytes());
    b.extend_from_slice(events);
    b
}

/// 完整 SMF 字节。
fn smf(format: u16, division: u16, tracks: &[Vec<u8>]) -> Vec<u8> {
    let mut b = b"MThd".to_vec();
    b.extend(6u32.to_be_bytes());
    b.extend(format.to_be_bytes());
    b.extend((tracks.len() as u16).to_be_bytes());
    b.extend(division.to_be_bytes());
    for t in tracks {
        b.extend_from_slice(t);
    }
    b
}

/// 拼接事件序列。
fn seq(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.concat()
}

// ── 用例 ──

#[test]
fn test_oracle_synthetic_all() {
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "running_status_note_chain",
            smf(
                0,
                480,
                &[track(&seq(&[
                    ev(0, &[0x90, 60, 100]),
                    ev(100, &[60, 0]), // running NoteOn vel0 = NoteOff
                    ev(0, &[62, 90]),
                    ev(100, &[62, 0]),
                    ev(0, &[0x80, 64, 30]), // 显式 NoteOff 切换 running status
                    ev(0, &[64, 0]),        // running NoteOff（非零释放力度）
                    eot(80),
                ]))],
            ),
        ),
        (
            "running_status_cc_chain",
            smf(
                0,
                480,
                &[track(&seq(&[
                    ev(0, &[0xB0, 7, 100]),
                    ev(10, &[7, 90]),
                    ev(10, &[10, 64]),
                    ev(10, &[0xB1, 7, 80]), // 换通道
                    ev(10, &[7, 70]),
                    eot(0),
                ]))],
            ),
        ),
        (
            "release_velocity_nonzero",
            smf(
                0,
                480,
                &[track(&seq(&[
                    ev(0, &[0x90, 60, 100]),
                    ev(480, &[0x80, 60, 64]),
                    eot(0),
                ]))],
            ),
        ),
        (
            "retrigger_same_key",
            smf(
                0,
                480,
                &[track(&seq(&[
                    ev(0, &[0x90, 60, 100]),
                    ev(100, &[0x90, 60, 90]), // 重触发：旧音符 release=0
                    ev(100, &[0x80, 60, 30]),
                    eot(0),
                ]))],
            ),
        ),
        (
            "same_tick_file_order",
            smf(
                0,
                480,
                &[track(&seq(&[
                    ev(0, &[0xB0, 1, 10]),
                    ev(0, &[0xB0, 1, 20]),
                    ev(0, &[0xB0, 1, 30]),
                    ev(0, &[0x90, 60, 100]),
                    ev(0, &[0x90, 62, 100]),
                    ev(0, &[0x90, 64, 100]),
                    ev(100, &[0x80, 62, 0]), // 先关 62 → 到达序在 60 前
                    ev(0, &[0x80, 60, 0]),
                    ev(0, &[0x80, 64, 0]),
                    eot(0),
                ]))],
            ),
        ),
        (
            "unclosed_note_with_eot",
            smf(
                0,
                480,
                &[track(&seq(&[
                    ev(0, &[0x90, 60, 100]),
                    eot(480), // 未闭合音符在 EOT tick 收尾
                ]))],
            ),
        ),
        (
            "unclosed_note_no_eot",
            smf(
                0,
                480,
                &[track(&seq(&[
                    ev(0, &[0x90, 60, 100]),
                    ev(200, &[0xB0, 7, 100]), // 最后事件 tick=200，无 EOT
                ]))],
            ),
        ),
        (
            "multi_track_different_eot",
            smf(
                1,
                480,
                &[
                    track(&seq(&[
                        meta(0, 0x51, &[0x07, 0xA1, 0x20]), // 120 BPM
                        ev(0, &[0x90, 60, 100]),
                        ev(400, &[0x80, 60, 0]),
                        eot(80), // track0 EOT@480
                    ])),
                    track(&seq(&[
                        ev(0, &[0x91, 64, 100]),
                        ev(900, &[0x81, 64, 0]),
                        eot(60), // track1 EOT@960
                    ])),
                ],
            ),
        ),
        (
            "channel_aftertouch_no_desync",
            smf(
                0,
                480,
                &[track(&seq(&[
                    ev(0, &[0xD0, 50]), // 0xD0 仅 1 数据字节（0.6.5 修复点）
                    ev(10, &[0x90, 60, 100]),
                    ev(10, &[0xD1, 60]),
                    ev(10, &[0xB0, 7, 100]),
                    ev(10, &[0x80, 60, 0]),
                    ev(10, &[0xC0, 5]), // 0xC0 仅 1 数据字节
                    ev(10, &[0xE0, 0, 64]),
                    eot(10),
                ]))],
            ),
        ),
        (
            "empty_track_eot_only",
            smf(
                1,
                480,
                &[
                    track(&eot(0)),
                    track(&seq(&[
                        ev(0, &[0x90, 60, 100]),
                        ev(100, &[0x80, 60, 0]),
                        eot(0),
                    ])),
                ],
            ),
        ),
        (
            "events_after_eot_lenient",
            smf(
                0,
                480,
                &[track(&seq(&[
                    ev(0, &[0x90, 60, 100]),
                    eot(400),
                    ev(80, &[0x80, 60, 0]), // EOT 后 NoteOff：当前宽松保留并完成配对
                ]))],
            ),
        ),
        (
            "sysex_and_escape",
            smf(
                0,
                480,
                &[track(&seq(&[
                    sysex(0, 0xF0, &[0x7D, 0x01, 0x02, 0xF7]),
                    sysex(10, 0xF7, &[0xAA, 0xBB]),
                    ev(10, &[0x90, 60, 100]),
                    ev(100, &[0x80, 60, 0]),
                    eot(0),
                ]))],
            ),
        ),
        (
            "text_meta_zoo_and_port_first_wins",
            smf(
                0,
                480,
                &[track(&seq(&[
                    meta(0, 0x03, b"TrackName"), // 不收录
                    meta(0, 0x01, b"Text"),
                    meta(0, 0x02, b"Copyright"),
                    meta(0, 0x04, b"Instrument"),
                    meta(0, 0x05, b"Lyric"),
                    meta(0, 0x06, b"Marker"),
                    meta(0, 0x07, b"Cue"),
                    meta(0, 0x08, b"Program"),
                    meta(0, 0x09, b"Device"),
                    meta(0, 0x21, &[3]),       // port 3（首个生效）
                    meta(0, 0x21, &[7]),       // port 7（忽略）
                    meta(0, 0x7F, &[1, 2, 3]), // 未知 meta：忽略
                    eot(0),
                ]))],
            ),
        ),
        (
            "tempo_zero_ignored",
            smf(
                0,
                480,
                &[track(&seq(&[
                    meta(0, 0x51, &[0, 0, 0]), // us=0：两侧均跳过
                    meta(0, 0x51, &[0x07, 0xA1, 0x20]),
                    meta(100, 0x51, &[0x06, 0x1A, 0x80]), // 150 BPM
                    eot(0),
                ]))],
            ),
        ),
        (
            "time_key_signatures",
            smf(
                0,
                480,
                &[track(&seq(&[
                    meta(0, 0x58, &[4, 2, 24, 8]),
                    meta(0, 0x59, &[0xFE, 1]), // -2 升号（2 降号）小调
                    meta(480, 0x58, &[3, 3, 24, 8]),
                    meta(0, 0x59, &[3, 0]),
                    eot(0),
                ]))],
            ),
        ),
        (
            "pitch_bend_boundaries",
            smf(
                0,
                480,
                &[track(&seq(&[
                    ev(0, &[0xE0, 0, 0]),        // 0
                    ev(10, &[0xE0, 0, 64]),      // 8192
                    ev(10, &[0xE0, 0x7F, 0x7F]), // 16383
                    eot(0),
                ]))],
            ),
        ),
        (
            "poly_aftertouch",
            smf(
                0,
                480,
                &[track(&seq(&[
                    ev(0, &[0xA0, 60, 50]),
                    ev(10, &[0xA1, 62, 60]),
                    eot(0),
                ]))],
            ),
        ),
        (
            "large_vlq_deltas",
            smf(
                0,
                480,
                &[track(&seq(&[
                    ev(16384, &[0x90, 60, 100]),
                    ev(2097152, &[0x80, 60, 0]),
                    eot(127),
                ]))],
            ),
        ),
        (
            "format2_sequential",
            smf(
                2,
                480,
                &[
                    track(&seq(&[
                        ev(0, &[0x90, 60, 100]),
                        ev(100, &[0x80, 60, 0]),
                        eot(0),
                    ])),
                    track(&seq(&[
                        ev(0, &[0x90, 62, 100]),
                        ev(100, &[0x80, 62, 0]),
                        eot(0),
                    ])),
                ],
            ),
        ),
        (
            "mixed_control_dense",
            smf(
                0,
                480,
                &[track(&seq(&[
                    ev(0, &[0xB0, 64, 127]), // sustain on
                    ev(0, &[0x90, 60, 100]),
                    ev(10, &[0xB0, 1, 50]),
                    ev(10, &[0xC0, 40]),
                    ev(10, &[0xE0, 0, 80]),
                    ev(10, &[0xD0, 70]),
                    ev(10, &[0xA0, 60, 80]),
                    ev(10, &[0x80, 60, 20]),
                    ev(10, &[0xB0, 64, 0]), // sustain off
                    eot(0),
                ]))],
            ),
        ),
    ];

    for (name, bytes) in &cases {
        run_oracle(bytes, name);
        eprintln!("[oracle] 合成用例 {name}: 对拍通过");
    }
}

/// 黄金断言：重触发语义的精确音符表（不只依赖参考链一致性，
/// 直接锁定文档语义，防止参考实现与快路径共同漂移）。
#[test]
fn test_oracle_golden_retrigger() {
    let bytes = smf(
        0,
        480,
        &[track(&seq(&[
            ev(0, &[0x90, 60, 100]),
            ev(100, &[0x90, 60, 90]),
            ev(100, &[0x80, 60, 30]),
            eot(0),
        ]))],
    );
    let mut got: Vec<(u32, u32, u8, u8, u8)> = Vec::new();
    midly::loader::extract_all_events_per_track_streaming_from_bytes(&bytes, |_idx, ev| {
        for n in &ev.notes {
            let n = *n;
            got.push((
                n.start_tick,
                n.end_tick,
                n.key,
                n.velocity,
                n.release_velocity,
            ));
        }
    })
    .expect("合成文件解析必须成功");
    assert_eq!(
        got,
        vec![(0, 100, 60, 100, 0), (100, 200, 60, 90, 30)],
        "重触发：旧音符 release=0，新音符透传 NoteOff 释放力度"
    );
}

/// 黄金断言：未闭合音符在 EOT tick 收尾（当前基线语义；
/// TrackTiming 实现后此用例升级为 §2.9 规格测试）。
#[test]
fn test_oracle_golden_unclosed_at_eot() {
    let bytes = smf(
        0,
        480,
        &[track(&seq(&[
            ev(0, &[0x90, 60, 100]),
            ev(0, &[0x91, 64, 90]),
            eot(480),
        ]))],
    );
    let mut got: Vec<(u32, u32, u8, u8)> = Vec::new();
    midly::loader::extract_all_events_per_track_streaming_from_bytes(&bytes, |_idx, ev| {
        for n in &ev.notes {
            let n = *n;
            got.push((n.start_tick, n.end_tick, n.channel, n.release_velocity));
        }
    })
    .expect("合成文件解析必须成功");
    assert_eq!(
        got,
        vec![(0, 480, 0, 0), (0, 480, 1, 0)],
        "未闭合音符按 channel×key 升序在 EOT tick 收尾，release=0"
    );
}
