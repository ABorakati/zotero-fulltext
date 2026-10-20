const std = @import("std");
pub fn build(b: *std.Build) void {
    const target = b.resolveTargetQuery(.{ .cpu_arch = .wasm32, .os_tag = .freestanding });
    for ([_]bool{ false, true }) |with_uri| {
        const opts = b.addOptions();
        opts.addOption(bool, "with_uri", with_uri);
        const mod = b.createModule(.{ .root_source_file = b.path("ladder.zig"), .target = target, .optimize = .ReleaseSmall });
        mod.addOptions("build_options", opts);
        const exe = b.addExecutable(.{ .name = if (with_uri) "zig-ladder-json-uri" else "zig-ladder-json", .root_module = mod });
        exe.entry = .disabled;
        exe.rdynamic = true;
        exe.root_module.strip = true;
        b.installArtifact(exe);
    }
}
