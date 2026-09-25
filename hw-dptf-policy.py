#!/usr/bin/env python3
"""hw-dptf-policy.py — Decode the DPTF/DTT policy that the firmware ships in the data vault.

Usage: ./hw-dptf-policy.py [data_vault]
       (default: /sys/bus/platform/devices/INTC1042:00/data_vault, readable without root)

The data vault (ACPI GDDV) holds the Intel DTT policy the OEM tuned for this machine:
power limits per mode (APCT conditions → APAT actions), passive tables (PSVT: sensor
temperature → power limit) and CPU power limit ranges (PPCC). Intel DTT applies it on
Windows; on Linux nothing does, but the EC follows the same thresholds.

Format taken from intel/thermal_daemon, src/thd_gddv.cpp.
"""

import lzma
import struct
import sys

DEFAULT_PATH = "/sys/bus/platform/devices/INTC1042:00/data_vault"

HEADER_SIGNATURE = 0x1FE5
KEY_SIGNATURE = 0xA0D8
FLAG_COMPRESSED = 0x40000000

# ESIF object types inside table values.
TYPE_U64 = 4
TYPE_BUFFER = 7
TYPE_STRING = 8

UNSET = 0xFFFFFFFF

# enum adaptive_condition in thd_gddv.cpp. Values from 0x1000 up are OEM-defined
# (an APPC table would name them; this machine has none).
CONDITIONS = [
    "Invalid", "Default", "Orientation", "Proximity", "Motion", "Dock", "Workload",
    "Cooling_mode", "Power_source", "Aggregate_power_percentage", "Lid_state",
    "Platform_type", "Platform_SKU", "Utilisation", "TDP", "Duty_cycle", "Power",
    "Temperature", "Display_orientation", "Oem0", "Oem1", "Oem2", "Oem3", "Oem4", "Oem5",
    "PMAX", "PSRC", "ARTG", "CTYP", "PROP", "Unk1", "Unk2", "Battery_state",
    "Battery_rate", "Battery_remaining", "Battery_voltage", "PBSS", "Battery_cycles",
    "Battery_last_full", "Power_personality", "Battery_design_capacity", "Screen_state",
    "AVOL", "ACUR", "AP01", "AP02", "AP10", "Time", "Temperature_without_hysteresis",
    "Mixed_reality", "User_presence", "RBHF", "VBNL", "CMPP", "Battery_percentage",
    "Battery_count", "Power_slider", "OS_Type",
]
# thermald only knows 1–4. This policy also uses 6, meaning unknown.
COMPARISONS = {1: "==", 2: "<=", 3: ">=", 4: "!="}
OPERATION_FOR = 2

CONTROL_KNOBS = {0x10000: "PL1"}


def u16(b, o):
    return struct.unpack_from("<H", b, o)[0]


def u32(b, o):
    return struct.unpack_from("<I", b, o)[0]


def u64(b, o):
    return struct.unpack_from("<Q", b, o)[0]


def deci_kelvin(v):
    return f"{(v - 2732) / 10:.1f} °C"


def read_items(buf):
    """Return the (key, value) pairs of a data vault, decompressing it if needed."""
    if u16(buf, 0) != HEADER_SIGNATURE:
        sys.exit(f"not a data vault (signature {u16(buf, 0):#06x})")
    header_size = u16(buf, 2)
    body = buf[header_size:]
    if u32(buf, 8) & FLAG_COMPRESSED:
        body = lzma.LZMADecompressor(format=lzma.FORMAT_ALONE).decompress(body)

    items, o = [], 0
    while o + 2 <= len(body):
        signature = u16(body, o)
        if signature == KEY_SIGNATURE:
            o += 2 + 4  # signature, key flags
            key_len = u32(body, o)
            o += 4
            key = body[o:o + key_len].rstrip(b"\0").decode()
            o += key_len + 4  # key, value type
            value_len = u32(body, o)
            o += 4
            items.append((key, body[o:o + value_len]))
            o += value_len
        elif signature == HEADER_SIGNATURE:
            o += u16(body, o + 2)  # nested segment: its keys follow its header
        elif not any(body[o:]):
            break
        else:
            sys.exit(f"unknown signature {signature:#06x} at offset {o}")
    return items


def objects(value):
    """Split a table value into its ESIF objects (integers, strings, buffers)."""
    out, o = [], 0
    while o + 4 <= len(value):
        kind = u32(value, o)
        o += 4
        if kind == TYPE_U64:
            out.append(u64(value, o))
            o += 8
        elif kind in (TYPE_STRING, TYPE_BUFFER):
            n = u64(value, o)
            o += 8
            data = value[o:o + n]
            o += n
            out.append(data.rstrip(b"\0").decode(errors="replace") if kind == TYPE_STRING else data)
        else:
            raise ValueError(f"unknown object type {kind} at offset {o - 4}")
    return out


def short(path):
    """\\_SB_.PC00.LPCB.EC__.SEN1 → SEN1"""
    return path.rsplit(".", 1)[-1]


def condition_name(c):
    return CONDITIONS[c] if c < len(CONDITIONS) else f"oem{c:#x}"


def print_psvt(name, value):
    t = objects(value)
    print(f"  {name}")
    i = 1  # skip the version
    while i < len(t):
        source, target, _prio, period, temp, _domain, knob, limit, _step, _lc, _uc, _res = t[i:i + 12]
        i += 12
        knob_name = CONTROL_KNOBS.get(knob, f"knob {knob:#x}")
        limit_str = f"{limit / 1000:g} W" if isinstance(limit, int) and knob_name == "PL1" else str(limit)
        print(f"    {short(target)} ≥ {deci_kelvin(temp)} → {short(source)} {knob_name} {limit_str}"
              f"  (every {period / 10:g} s)")


def print_ppcc(name, value):
    t = objects(value)
    print(f"  {name}")
    for i in range(1, len(t), 6):  # skip the version
        index, pmin, pmax, twmin, twmax, step = t[i:i + 6]
        if pmin == UNSET:
            continue
        print(f"    limit {index}: {pmin / 1000:g}–{pmax / 1000:g} W, "
              f"time window {twmin / 1000:g}–{twmax / 1000:g} s, step {step / 1000:g} W")


def read_rules(apct, apat):
    """Join APCT (target → conditions) and APAT (target → actions)."""
    actions = {}
    t = objects(apat)
    for i in range(1, len(t), 6):  # version 2
        target_id, name, _participant, _domain, code, argument = t[i:i + 6]
        actions.setdefault(target_id, (name, []))[1].append(f"{code}={argument}")

    rules = []
    t = objects(apct)
    if t[0] != 2:
        sys.exit(f"unsupported APCT version {t[0]}")
    i = 1
    while i < len(t):
        target_id, count = t[i], t[i + 1]
        i += 2
        conditions, k = [], 0
        while k < count:
            cond, device, _domain, comp, arg = t[i:i + 5]
            i += 5
            text = f"{condition_name(cond)}[{short(device)}] {COMPARISONS.get(comp, f'op{comp}')} "
            text += deci_kelvin(arg) if condition_name(cond).startswith("Temperature") else str(arg)
            if k < count - 1:
                operation = t[i]
                i += 1
                if operation == OPERATION_FOR:
                    _, _, _, time_comp, time, _ = t[i:i + 6]
                    i += 6
                    text += f" for {COMPARISONS.get(time_comp, '?')} {time / 10:g} s"
                    k += 1
            if cond != 1:  # "Default" pads the fixed-size condition lists
                conditions.append(text)
            k += 1
        name, acts = actions.get(target_id, ("?", []))
        rules.append((target_id, name, conditions, acts))
    return rules


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else DEFAULT_PATH
    items = dict(read_items(open(path, "rb").read()))

    print("== Features")
    for key, value in items.items():
        if key.startswith("/features/"):
            print(f"  {key.rsplit('/', 1)[-1]} = {u32(value, 0)}")

    print("\n== Rules (APCT → APAT), first match wins")
    for target_id, name, conditions, acts in read_rules(items["/shared/export/apct"],
                                                        items["/shared/export/apat"]):
        print(f"  [{target_id}] {name}")
        print(f"    if   {' and '.join(conditions) or 'always'}")
        print(f"    then {', '.join(acts)}")

    print("\n== Passive tables (PSVT)")
    for key, value in items.items():
        if key.endswith("/psvt"):
            print_psvt(key, value)
        elif "/psvt/" in key:
            print_psvt(key.rsplit("/", 1)[-1], value)

    print("\n== CPU power limit ranges (PPCC)")
    for key, value in items.items():
        if key.endswith("/ppcc"):
            print_ppcc(key, value)

    print("\n== Trip points set by the policy (unset ones fall back to ACPI)")
    trips = [(k, u32(v, 0)) for k, v in items.items() if "/trippoint/" in k]
    for key, value in trips:
        if value != UNSET:
            print(f"  {key} = {deci_kelvin(value)}")
    if all(v == UNSET for _, v in trips):
        print("  none")


if __name__ == "__main__":
    main()
