// Zig 0.16 equivalent of crates/ladder (--features json[,url]): same hand ABI.
const std = @import("std");
const gpa = std.heap.wasm_allocator;
const with_uri = @import("build_options").with_uri;

export fn fx_alloc(len: usize) ?[*]u8 {
    const s = gpa.alloc(u8, len) catch return null;
    return s.ptr;
}
export fn fx_free(ptr: [*]u8, len: usize) void {
    gpa.free(ptr[0..len]);
}
fn pack(s: []u8) u64 {
    return (@as(u64, @intFromPtr(s.ptr)) << 32) | s.len;
}

const In = struct { url: []const u8, base: ?[]const u8 = null, n: ?f64 = null };

export fn fx_echo(ptr: [*]const u8, len: usize) u64 {
    const parsed = std.json.parseFromSlice(In, gpa, ptr[0..len], .{}) catch {
        const msg = gpa.dupe(u8, "{\"err\":\"bad_json\"}") catch return 0;
        return pack(msg);
    };
    defer parsed.deinit();
    var v = parsed.value;
    var buf: [2048]u8 = undefined;
    if (with_uri) if (v.base) |b| {
        if (std.Uri.parse(b)) |base| {
            if (v.url.len <= buf.len) {
                @memcpy(buf[0..v.url.len], v.url);
                var aux: []u8 = buf[0..];
                if (std.Uri.resolveInPlace(base, v.url.len, &aux)) |r| {
                    _ = r; // a real port would format `r` back to a string here
                    v.url = "resolved";
                } else |_| {}
            }
        } else |_| {}
    };
    const out = std.json.Stringify.valueAlloc(gpa, v, .{}) catch return 0;
    return pack(out);
}
