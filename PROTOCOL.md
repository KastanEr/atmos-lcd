# Cooler Master MasterLiquid Atmos II LCD — USB protocol

Reverse engineered from MasterCTRL 1.5.2.244 (`CMAgent.exe`) and a USBPcap capture.
Verified on hardware with `AtmosLcd.cs`.

## Device

- USB `2516:0222`, product string `ATMOS V2-LCD`, LCD 480x480.
- Interface MI_00: vendor HID (usage page `0xFFFF`), interrupt OUT `0x01`, IN `0x81`.
- Interface MI_01: a 4-byte HID mouse, unused.
- Reports on MI_00:

| ID | Dir | Size | Use |
|----|-----|------|-----|
| `0x02` | out (SET_REPORT) | 1 | host protocol enable, value `0x04` |
| `0x1E` | out | 1023 | text command |
| `0x1F` | out | 1023 | data chunk |
| `0x20` | in | 1023 | response |
| `0x01` | feature | 6 | `02 00 01 00 00` (version?) |
| `0x14` | feature | 47 | TLV capabilities: 480, 480, chunk 1000, ... |

## Handshake

Send output report `[0x02, 0x04]` via `HidD_SetOutputReport` once after opening.
Without it the device silently ignores every command.

## Command (`0x1E`) and response (`0x20`) framing

```
[id][0x5A][len hi][len lo][text ...][checksum][0x5A][00 00 00 ...pad to 1024]
len      = text length + 5
checksum = (sum of bytes from len hi through the last text byte) & 0xFF
```

Text is HTTP-like:

```
POST <name> 1\r\nSeqNumber=<n>\r\nDate=<unix s>\r\nContentType=json\r\nContentLength=<n>\r\n\r\n<json>
```

The parameter query is just `POST param 1\r\n`.

Responses look like `1 200\r\nAckNumber=<seq+1>\r\n...\r\n\r\n<json>`. The firmware inserts
one stray control byte every 128 body bytes, and neither length field counts it.

## Commands seen

| Command | Seq | Body | Notes |
|---------|-----|------|-------|
| `POST param` | – | – | `{"brightness":80,"degree":0,"osdState":0,"keepAliveTimeout":5,"maxSuspendMediaCount":5,"displayInSleep":0,"suspendMediaActive":[1,0,0,0,0]}` |
| `POST getSKUColor` | – | – | `{"color":"1"}` |
| `POST brightness` | 44 | `{"value":0-100}` | |
| `POST rotate` | 45 | `{"degree":0/90/180/270}` | from binary, not captured |
| `POST realtimeDisplay` | 42 | `{"enable":true}` | resend every 4 s while streaming |
| `STATE timestamp` | 42 | `{"timestamp":<unix ms>}` | sent before each realtimeDisplay keepalive |
| `POST transport` | 42 | `{"fileName":"logo_0.mp4","fileSize":N,"type":"logo"}` | reply `{"state":"success","blockMaxSize":1024}`, then data chunks with flag 0 |
| `POST transported` | 42 | `{"fileName":"...","md5":1111111}` | finishes an upload |
| `POST delMedia` | 42 | `{"fileName":"suspend_N.mp4","type":"suspend"}` | verified; `"fileName":"all"` clears every slot (from binary) |
| `POST displayInSleep` | 42 | `{"enable":bool}` | from binary: show stored media while the PC sleeps |

Upload types seen in the binary: `logo` (`logo_0.mp4`) and `suspend` (`suspend_<n>.mp4` / `.jpg`,
up to 5). These play offline when no host is streaming. MP4 is H.264 yuv420p.

## Data chunk (`0x1F`)

```
[0]    0x1F
[1]    0x5C
[2..3] payload length + 20 (big endian)
[4]    session byte, random 0..59, same for all chunks of one frame/file
[5..6] total chunk count (BE)
[7..8] chunk index (BE)
[9]    flag: 1 = realtime frame, 0 = file upload
[10..23] zero
[24..] payload, up to 1000 bytes
```

## Realtime display

1. `STATE timestamp` + `POST realtimeDisplay {"enable":true}`
2. Stream baseline JPEGs (480x480, about 15–80 KB) as `0x1F` chunks with flag 1.
   MasterCTRL runs at about 13 fps.
3. Repeat step 1 every 4 s. After about 5 s with no keepalive, the device falls back to its stored media.

## Stored media (offline playback)

The device keeps up to `maxSuspendMediaCount` (5) files named `suspend_<0-4>.jpg` or
`suspend_<0-4>.mp4`, plus a boot logo `logo_0.mp4`. `param` reports which slots are
filled in `suspendMediaActive`, e.g. `[1,1,0,0,0]`. When no host streams (or after
`realtimeDisplay {"enable":false}`), the device plays all filled slots in turn.
No command selects a single slot; keeping one means deleting the others.

Storing a file into slot N (MasterCTRL's order, verified on hardware):

1. `delMedia` for `suspend_N.mp4` and `suspend_N.jpg` (60 ms apart).
2. `transport {"fileName":"suspend_N.mp4","fileSize":<bytes>,"type":"suspend"}` →
   `{"state":"success","blockMaxSize":1024}`; wait 300 ms.
3. The file as `0x1F` data chunks with flag 0; wait 500 ms. The device answers
   `AckNumber=0` with no body.
4. `transported {"fileName":"suspend_N.mp4","md5":1111111}` → `{"state":"success"}`.
   MasterCTRL always sends that placeholder checksum.

Accepted formats: 480x480 baseline JPEG; H.264 MP4 at 480x480 (Windows' Media
Foundation encoder, baseline profile, ~2 Mbps; `moov` after `mdat` plays fine).
Storage appears to be about 20 MB (`0x01400000` in feature report 0x14), not yet verified.

Rotation for stored media follows the firmware `rotate` setting (90° steps); other
angles must be applied to the file before uploading.

## Throughput

Writing data reports one at a time (waiting for each) gives ~16 ms per 54 KB frame.
Keeping two or more writes in flight gives ~8.8 ms (≈ 6 MB/s, ~110 fps); that is the
device's limit. The panel plays 60 fps smoothly.
