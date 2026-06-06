# MQTT integration

## Topics

| Topic | Published | Payload |
|-------|-----------|---------|
| `zappy/boot` | Once on connect | `"1"` |
| `zappy/zap` | On each zap event | Cumulative count as decimal string, e.g. `"42"` |

The zap count is **cumulative per boot** — it starts at `0` on each connect and increments. It resets to `0` after a reboot/reconnect.

## MQTT Discovery

On connect, the firmware publishes HA MQTT discovery config messages to:
- `homeassistant/sensor/zappy/zap/config`
- `homeassistant/sensor/zappy/boot/config`

This auto-creates two entities in Home Assistant under a device named **Zappy**:
- `sensor.zappy_zappy_zap_count` — `state_class: total_increasing`
- `sensor.zappy_zappy_boot`

## Home Assistant

HA is configured to forward these sensors to InfluxDB for long-term storage.

Because the zap count resets on each boot, use the Flux `increase()` function when querying
InfluxDB — it stitches the per-boot segments into a monotonically increasing total:

```flux
from(bucket: "homeassistant")
  |> range(start: v.timeRangeStart, stop: v.timeRangeStop)
  |> filter(fn: (r) => r["entity_id"] == "zappy_zappy_zap_count")
  |> filter(fn: (r) => r["_field"] == "value")
  |> toFloat()
  |> increase()
```

A Grafana dashboard with pre-built panels (raw count, total with resets handled, boot events)
is available on the monitoring server.

## Client ID

The firmware connects with MQTT client ID `zappy`. Only one connection with this ID can be
active at a time — a new connect will take over and disconnect the previous session.

## Credentials

See `secrets.env` (not committed).
