# deploy/events — agent 事件订阅(walgit events 桥的消费侧)

契约的规范文本在 [`docs/EVENTS.md`](../../docs/EVENTS.md)(D32);本目录是
**agent 侧的参考消费实现**:`agent-receiver.py`,一个零依赖(python3 stdlib)
的 webhook 接收器——跑起来即订阅。

## 启用(本部署)

1. `walgit.toml` 加(roles 留空的单机部署,桥随服务一起跑):

   ```toml
   [events]
   webhook_url = "http://127.0.0.1:8099/walgit"
   # webhook_secret = "…"            # 建议配置;接收器以 WALGIT_EVENTS_SECRET 对验
   sweep_interval = "5m"             # 没有 bucket 通知时,这就是事件延迟上限
   ```

2. 重启服务(`walgit service restart --config ~/.walgit/walgit.toml`)。

## 运行接收器

```bash
WALGIT_EVENTS_SECRET=<桥配置的 secret> \
python3 deploy/events/agent-receiver.py --port 8099
```

- 事件追加到 `~/walgit/events.jsonl`(每行一个 `ref` 事件,含 `_walgit.seq`)
- 批级去重:`X-Walgit-Delivery` 已见过的批次 ack 但不重复落盘
- 验签:配置了 secret 时,`X-Walgit-Signature`(sha256 HMAC)常量时间比对

## 语义(契约要点)

- **at-least-once**:接收器不回 2xx(崩溃/500/超时),桥在下次唤醒重投同一
  批次——重复可能,事件不丢;接收器按 `X-Walgit-Delivery` 去重
- **回放/补洞**:桥落后于日志保留窗造成缺口时,从最后已知 seq 起用
  `walgit wal ls` 读 WAL 补齐(PUSH/REF_UPDATE 条目即事件)——webhook 只是
  延迟优化,正确性从不依赖它
- **顺序**:单 repo 内按 `_walgit.seq` 升序;跨 repo 无顺序承诺

## 验证(at-least-once 与回放,试点脚本)

```bash
# 1) 推一个提交 → events.jsonl 立刻出现 ref 事件(create/update)
git push origin main && tail -1 ~/walgit/events.jsonl

# 2) at-least-once:停接收器 → 再推一个 → 重启接收器
#    (桥的 sweep 重投同一批,重启后的接收器应补收且不重复)
```
