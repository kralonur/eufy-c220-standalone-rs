.syntax unified
.arm
.text
.global _start
.type _start,%function

/* Minimal camera-compatible entry point used by uClibc-linked helpers.
 * The vendor camera already provides the dynamic loader/libc; this avoids
 * shipping a second runtime or startup library. */
_start:
    mov r4, sp
    ldr r0, [r4]
    add r1, r4, #4
    add r2, r1, r0, lsl #2
    add r2, r2, #4
    bl main
    mov r7, #1
    svc #0
.size _start, .-_start
