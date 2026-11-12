// The dashboard ships inside the Rust binary: `next build` emits a static
// export into `out/`, and `npm run sync` copies that over `web/dist/`, which
// `crates/lidlesseye-server/src/ui.rs` embeds with rust-embed. Keeping the
// output directory stable means `cargo build` still works without Node.
/** @type {import('next').NextConfig} */
const isDev = process.env.NODE_ENV === "development";

// `output: "export"` cannot coexist with `rewrites`, so the dev server drops the
// export and proxies the API to a running agent instead. Without this every
// request under `npm run dev` would hit port 3000 and 404, which would leave
// `next build` as the only way to look at a change.
const devApiTarget = process.env.LIDLESSEYE_DEV_SERVER ?? "http://127.0.0.1:4040";

const nextConfig = {
  ...(isDev ? {} : { output: "export" }),
  // No image server exists in the static export.
  images: { unoptimized: true },
  // Every view is a hash route (`#/flamegraph?…`), so there is exactly one
  // HTML entry point and no directory index to resolve.
  trailingSlash: false,
  reactStrictMode: true,
  eslint: { ignoreDuringBuilds: true },
  ...(isDev
    ? {
        async rewrites() {
          return [
            { source: "/api/:path*", destination: `${devApiTarget}/api/:path*` },
            { source: "/ingest", destination: `${devApiTarget}/ingest` },
            { source: "/metrics", destination: `${devApiTarget}/metrics` },
          ];
        },
      }
    : {}),
};

export default nextConfig;
