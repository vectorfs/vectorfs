#!/usr/bin/env python3
"""Regenerate the tiny-siglip2 ONNX fixtures (M3-T2, the image-modality twin
of tiny-embedder/generate.py — same hermetic discipline: committed artifacts
were produced by this script; rerun only when the fixture shape must change,
every test that pins embedding VALUES will need updating too).

The fixtures mirror the I/O contract of the REAL pinned artifact
(onnx-community/siglip2-base-patch16-224-ONNX, split-tower layout, verified
2026-11-06 against the actual graphs):

  vision tower  `vision.onnx`
    IN  pixel_values FLOAT [batch, 3, height, width]   (real: dynamic H/W;
        fixture: STATIC 8x8 so the session-metadata probe path is exercised)
    OUT pooler_output FLOAT [batch, 8]                 (static last dim = the
        startup dim probe target)
    graph: GlobalAveragePool -> Reshape(-1,3) -> MatMul(3x8 const) — channel
    means projected by a fixed arithmetic pattern; tiny, deterministic, zero
    randomness (committed bytes reproducible bit-for-bit).

  text tower  `text.onnx`
    IN  input_ids INT64 [batch, seq]                   (the real export takes
        ONLY input_ids — no attention_mask; the fixture matches)
    OUT pooler_output FLOAT [batch, 8]
    graph: Gather over a 16x8 const table (id -> row) -> ReduceMean over the
    sequence axis (the runtime never pools: the GRAPH produces the pooled
    row, exactly like the real export's EOS-gather + head-Gemm).

  tokenizer.json — the same minimal WordLevel 16-token vocab as
    tiny-embedder ([CLS]/[SEP] template), i.e. the same tokenizers-crate code
    path the real 256k-vocab SigLIP2 tokenizer.json takes, minus the size.

Usage: python3 generate.py <output-dir>   (default: alongside this script)
"""

import json
import pathlib
import sys

import onnx
from onnx import TensorProto, helper, numpy_helper

import numpy as np

VOCAB_SIZE = 16
DIM = 8
SIZE = 8  # fixture input resolution: 8x8 (the kind probes it from the graph)

# identical to tiny-embedder/generate.py so both fixtures share one tokenizer
# contract (WordLevel, [CLS]/[SEP] template; CJK row -> [UNK] by design)
VOCAB = {
    "[PAD]": 0,
    "[UNK]": 1,
    "[CLS]": 2,
    "[SEP]": 3,
    "contract": 4,
    "probe": 5,
    "first": 6,
    "chunk": 7,
    "second": 8,
    "hello": 9,
    "vectorfs": 10,
    "the": 11,
    "quick": 12,
    "brown": 13,
    "fox": 14,
    "embeds": 15,
}


def pattern_matrix() -> np.ndarray:
    """The house arithmetic pattern (tiny-embedder's): row[v][d] =
    ((v * 8 + d) % 16) / 8 - 1 in [-1, 1). Used for both the text table and
    the vision projection so expected vectors are hand-computable."""
    m = np.empty((VOCAB_SIZE, DIM), dtype=np.float32)
    for v in range(VOCAB_SIZE):
        for d in range(DIM):
            m[v][d] = ((v * 8 + d) % 16) / 8.0 - 1.0
    return m


def build_vision_model(flat: bool = False) -> onnx.ModelProto:
    """`flat=True` builds the dynamic-dim twin (tiny-embedder's
    `model_flat.onnx` discipline): MatMul -> Reshape(-1), a rank-1 output
    whose only dim is genuinely runtime-dependent — ONNX shape inference
    cannot pin a static last dim, so the fixture exercises the siglip2
    kind's "dim cannot be probed" startup failure against the real runtime."""
    pixels = helper.make_tensor_value_info(
        "pixel_values",
        TensorProto.FLOAT,
        ["batch", 3, SIZE, SIZE],  # static H/W: the fixture probes 8x8
    )
    # the output stays NAMED `pooler_output` in both twins: the flat one
    # must pass the pooled-outlet LOOKUP and fail at the DIM PROBE (a
    # differently-named output would fail one step too early)
    out = helper.make_tensor_value_info(
        "pooler_output",
        TensorProto.FLOAT,
        ["batch", DIM] if not flat else ["flat"],
    )

    proj = numpy_helper.from_array(pattern_matrix()[:3], name="vision_proj")  # [3,8]
    shape3 = numpy_helper.from_array(np.array([-1, 3], dtype=np.int64), name="flat3")

    nodes = [
        helper.make_node("GlobalAveragePool", ["pixel_values"], ["chan_means"]),
        helper.make_node("Reshape", ["chan_means", "flat3"], ["means_2d"]),
    ]
    inits = [proj, shape3]
    if flat:
        nodes.append(
            helper.make_node("MatMul", ["means_2d", "vision_proj"], ["pooled_2d"])
        )
        shape1 = numpy_helper.from_array(np.array([-1], dtype=np.int64), name="flat1")
        nodes.append(helper.make_node("Reshape", ["pooled_2d", "flat1"], ["pooler_output"]))
        inits.append(shape1)
    else:
        nodes.append(
            helper.make_node("MatMul", ["means_2d", "vision_proj"], ["pooler_output"])
        )

    graph = helper.make_graph(
        nodes,
        "tiny-siglip2-vision" + ("-flat" if flat else ""),
        [pixels],
        [out],
        initializer=inits,
    )
    model = helper.make_model(
        graph,
        opset_imports=[helper.make_opsetid("", 17)],
        producer_name="vectorfs fixture generator",
    )
    # onnx 1.23 writes IR 14; ONNX Runtime 1.31 (the pinned runtime) accepts
    # at most IR 13 — pin explicitly (same note as tiny-embedder/generate.py).
    model.ir_version = 9
    onnx.checker.check_model(model)
    return model


def build_text_model() -> onnx.ModelProto:
    ids = helper.make_tensor_value_info(
        "input_ids", TensorProto.INT64, ["batch", "seq"]
    )
    out = helper.make_tensor_value_info(
        "pooler_output", TensorProto.FLOAT, ["batch", DIM]
    )

    table = numpy_helper.from_array(pattern_matrix(), name="text_table")  # [16,8]

    gather = helper.make_node(
        "Gather", ["text_table", "input_ids"], ["rows"], axis=0
    )
    reduce = helper.make_node(
        "ReduceMean", ["rows"], ["pooler_output"], axes=[1], keepdims=0
    )

    graph = helper.make_graph(
        [gather, reduce],
        "tiny-siglip2-text",
        [ids],
        [out],
        initializer=[table],
    )
    model = helper.make_model(
        graph,
        opset_imports=[helper.make_opsetid("", 17)],
        producer_name="vectorfs fixture generator",
    )
    model.ir_version = 9
    onnx.checker.check_model(model)
    return model


def build_tokenizer() -> dict:
    return {
        "version": "1.0",
        "truncation": None,
        "padding": None,
        "added_tokens": [
            {"id": i, "content": tok, "single_word": False, "lstrip": False,
             "rstrip": False, "normalized": False, "special": True}
            for tok, i in VOCAB.items() if tok.startswith("[") and tok.endswith("]")
        ],
        "normalizer": {"type": "Lowercase"},
        "pre_tokenizer": {"type": "Whitespace"},
        "post_processor": {
            "type": "TemplateProcessing",
            "single": [
                {"SpecialToken": {"id": "[CLS]", "type_id": 0}},
                {"Sequence": {"id": "A", "type_id": 0}},
                {"SpecialToken": {"id": "[SEP]", "type_id": 0}},
            ],
            "pair": [
                {"SpecialToken": {"id": "[CLS]", "type_id": 0}},
                {"Sequence": {"id": "A", "type_id": 0}},
                {"SpecialToken": {"id": "[SEP]", "type_id": 0}},
                {"Sequence": {"id": "B", "type_id": 1}},
                {"SpecialToken": {"id": "[SEP]", "type_id": 1}},
            ],
            "special_tokens": {
                "[CLS]": {"id": "[CLS]", "ids": [2], "tokens": ["[CLS]"]},
                "[SEP]": {"id": "[SEP]", "ids": [3], "tokens": ["[SEP]"]},
            },
        },
        "decoder": None,
        "model": {
            "type": "WordLevel",
            "vocab": VOCAB,
            "unk_token": "[UNK]",
        },
    }


def main() -> None:
    out_dir = pathlib.Path(
        sys.argv[1] if len(sys.argv) > 1 else pathlib.Path(__file__).parent
    )
    out_dir.mkdir(parents=True, exist_ok=True)

    onnx.save(build_vision_model(), out_dir / "vision.onnx")
    onnx.save(build_vision_model(flat=True), out_dir / "vision_flat.onnx")
    onnx.save(build_text_model(), out_dir / "text.onnx")

    with open(out_dir / "tokenizer.json", "w", encoding="utf-8") as f:
        json.dump(build_tokenizer(), f, indent=2, ensure_ascii=False)
        f.write("\n")

    with open(out_dir / "config.json", "w", encoding="utf-8") as f:
        json.dump(
            {
                "model_type": "tiny-siglip2-fixture",
                "projection_dim": DIM,
                "image_size": SIZE,
                "text_vocab_size": VOCAB_SIZE,
                "license": "CC0-1.0 (vectorfs test fixture, generated in-repo)",
            },
            f,
            indent=2,
        )
        f.write("\n")

    print(f"wrote {out_dir}/vision.onnx text.onnx tokenizer.json config.json")


if __name__ == "__main__":
    main()
