// The dashboard ships inside the Rust binary: `next build` emits a static
// export into `out/`, and `npm run sync` copies that over `web/dist/`, which
// `crates/lidlesseye-server/src/ui.rs` embeds with rust-embed. Keeping the
// output directory stable means `cargo build` still works without Node.
/** @type {import('next').NextConfig} */
const nextConfig = {
  output: "export",
  // No image server exists in the static export.
  images: { unoptimized: true },
  // Every view is a hash route (`#/flamegraph?…`), so there is exactly one
  // HTML entry point and no directory index to resolve.
  trailingSlash: false,
  reactStrictMode: true,
  eslint: { ignoreDuringBuilds: true },
};

export default nextConfig;
