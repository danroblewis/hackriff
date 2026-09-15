// Packet inspector (ADR-0013 §4.7). Owner: T-154, which replaces this placeholder mount.
import type { MountFn } from "../context";
import { placeholder } from "../placeholder";

export const mountInspector: MountFn = placeholder("Output stream", "T-154", "Frames, bytes and fields.");
