<!-- K0 discipline (design.md §10): every new component answers the three
     questions before review. Keep answers short; link evidence. -->

## The three questions

1. **Is this differentiating-necessary, or just cheap because AI wrote it?**
   <!-- What user-facing problem does this solve that no neighbor already
        solves? If the honest answer is "it was easy to add", cut it. -->

2. **Does it add persistent state without user evidence?**
   <!-- New on-disk formats, new long-lived processes, new config that users
        must maintain — all need existing demand, not speculation (G4). -->

3. **Is the documentation still ≤ the code it documents?**
   <!-- Count lines on both sides when this PR adds prose (G6). -->

## Acceptance steps

<!-- Exact commands a reviewer can copy-paste to verify this PR (conformance
     scripts, cargo test filters, manual SDK roundtrips). CI green alone is
     not acceptance for protocol-facing changes. -->

```
# 1. ...
# 2. ...
```
