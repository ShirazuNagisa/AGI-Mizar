#!/usr/bin/env python3
"""Merge the LoRA adapter into Qwen3-1.7B and write a 4-bit GGUF.

    pip install transformers peft torch sentencepiece gguf
    python export_gguf.py --adapter ../../../../tools/replay/out/mizarllm-v1 --out ./mizarllm-v1

Produces `<out>/mizarllm-v1-f16.gguf` and, if `llama-quantize` is on PATH,
`<out>/mizarllm-v1-q4.gguf`. The kernel loads the quantised one.
"""
import argparse
import os
import shutil
import subprocess
import sys


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", default="Qwen/Qwen3-1.7B")
    ap.add_argument("--adapter", required=True)
    ap.add_argument("--out", default="./mizarllm-v1")
    ap.add_argument("--skip-quantize", action="store_true")
    args = ap.parse_args()

    import torch
    from peft import PeftModel
    from transformers import AutoModelForCausalLM, AutoTokenizer

    os.makedirs(args.out, exist_ok=True)
    merged = os.path.join(args.out, "merged")

    print(f"== loading {args.base}")
    model = AutoModelForCausalLM.from_pretrained(args.base, torch_dtype=torch.float16, device_map="cpu")
    tok = AutoTokenizer.from_pretrained(args.adapter)

    print(f"== attaching adapter {args.adapter}")
    model = PeftModel.from_pretrained(model, args.adapter)
    model = model.merge_and_unload()

    print(f"== saving merged weights to {merged}")
    model.save_pretrained(merged, safe_serialization=True)
    tok.save_pretrained(merged)

    f16 = os.path.join(args.out, "mizarllm-v1-f16.gguf")
    convert = find_convert()
    if not convert:
        print("!! llama.cpp convert_hf_to_gguf.py not found; merged HF weights are in", merged)
        print("   install: pip install 'git+https://github.com/ggml-org/llama.cpp'  (or clone the repo)")
        return 1

    print("== converting to GGUF (f16) — this takes a few minutes")
    subprocess.run([sys.executable, convert, merged, "--outfile", f16, "--outtype", "f16"], check=True)

    if args.skip_quantize:
        return 0
    quantize = shutil.which("llama-quantize")
    if not quantize:
        print("!! llama-quantize not on PATH; reference run without quantisation:", f16)
        return 0
    q4 = os.path.join(args.out, "mizarllm-v1-q4.gguf")
    print("== quantising to Q4_K_M")
    subprocess.run([quantize, f16, q4, "Q4_K_M"], check=True)
    print("== ready:", q4, f"({os.path.getsize(q4) / 2**30:.2f} GB)")
    return 0


def find_convert() -> str | None:
    for cand in ("convert_hf_to_gguf.py", "llama.cpp/convert_hf_to_gguf.py"):
        if os.path.exists(cand):
            return os.path.abspath(cand)
    return None


if __name__ == "__main__":
    raise SystemExit(main())
