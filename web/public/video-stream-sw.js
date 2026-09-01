const STREAM_PATH_PREFIX = "/__autonomi_stream/";
const STREAM_BLOCK_BYTES = 1024 * 1024;
const RANGE_REQUEST_TIMEOUT_MS = 45_000;
const MAX_STREAM_FILE_BYTES = 1_000_000_000;

self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (event) => event.waitUntil(self.clients.claim()));

self.addEventListener("fetch", (event) => {
  const url = new URL(event.request.url);
  if (url.origin !== self.location.origin || !url.pathname.startsWith(STREAM_PATH_PREFIX)) {
    return;
  }
  event.respondWith(handleStreamRequest(event, url));
});

async function handleStreamRequest(event, url) {
  const sessionId = decodeURIComponent(
    url.pathname.slice(STREAM_PATH_PREFIX.length).split("/", 1)[0],
  );
  const size = Number(url.searchParams.get("size"));
  const requestedContentType = url.searchParams.get("type") || "";
  const contentType =
    requestedContentType.length <= 255 && !/[\0\r\n]/u.test(requestedContentType)
      ? requestedContentType || "application/octet-stream"
      : "application/octet-stream";
  const name = url.searchParams.get("name") || "autonomi-video";
  if (
    !/^[a-f0-9]{32}$/u.test(sessionId) ||
    !Number.isSafeInteger(size) ||
    size <= 0 ||
    size > MAX_STREAM_FILE_BYTES
  ) {
    return new Response("Invalid Autonomi stream URL", { status: 400 });
  }

  const parsed = parseRange(event.request.headers.get("range"), size);
  if (!parsed) {
    return new Response(null, {
      status: 416,
      headers: {
        "Accept-Ranges": "bytes",
        "Content-Range": `bytes */${size}`,
        "Cache-Control": "no-store",
      },
    });
  }
  const { start, end, partial } = parsed;
  const responseLength = end - start + 1;
  const headers = new Headers({
    "Accept-Ranges": "bytes",
    "Cache-Control": "no-store",
    "Content-Disposition": `inline; filename*=UTF-8''${encodeURIComponent(name)}`,
    "Content-Length": String(responseLength),
    "Content-Type": contentType,
  });
  if (partial) headers.set("Content-Range", `bytes ${start}-${end}/${size}`);
  if (event.request.method === "HEAD") {
    return new Response(null, { status: partial ? 206 : 200, headers });
  }
  if (event.request.method !== "GET") {
    return new Response("Method not allowed", { status: 405 });
  }

  let offset = start;
  const body = new ReadableStream({
    async pull(controller) {
      if (offset > end) {
        controller.close();
        return;
      }
      const length = Math.min(STREAM_BLOCK_BYTES, end - offset + 1);
      try {
        const bytes = await requestRangeFromPage(
          event.clientId,
          sessionId,
          offset,
          length,
        );
        if (bytes.byteLength !== length) {
          throw new Error(
            `range reader returned ${bytes.byteLength} bytes, expected ${length}`,
          );
        }
        controller.enqueue(bytes);
        offset += bytes.byteLength;
      } catch (error) {
        controller.error(error);
      }
    },
  });
  return new Response(body, { status: partial ? 206 : 200, headers });
}

function parseRange(header, size) {
  if (!header) return { start: 0, end: size - 1, partial: false };
  const match = /^bytes=(\d*)-(\d*)$/u.exec(header.trim());
  if (!match || (!match[1] && !match[2])) return undefined;

  let start;
  let end;
  if (!match[1]) {
    const suffixLength = Number(match[2]);
    if (!Number.isSafeInteger(suffixLength) || suffixLength <= 0) return undefined;
    start = Math.max(0, size - suffixLength);
    end = size - 1;
  } else {
    start = Number(match[1]);
    end = match[2] ? Number(match[2]) : size - 1;
    if (!Number.isSafeInteger(start) || !Number.isSafeInteger(end)) return undefined;
    if (start >= size || end < start) return undefined;
    end = Math.min(end, size - 1);
  }
  return { start, end, partial: true };
}

async function requestRangeFromPage(clientId, sessionId, start, length) {
  let client = clientId ? await self.clients.get(clientId) : undefined;
  if (!client) {
    const candidates = await self.clients.matchAll({
      type: "window",
      includeUncontrolled: true,
    });
    client = candidates[0];
  }
  if (!client) throw new Error("No browser page is available to serve the video range");

  return new Promise((resolve, reject) => {
    const channel = new MessageChannel();
    const fail = (error) => {
      clearTimeout(timeout);
      channel.port1.close();
      reject(error);
    };
    const timeout = setTimeout(() => {
      fail(new Error("Autonomi video range request timed out"));
    }, RANGE_REQUEST_TIMEOUT_MS);
    channel.port1.onmessage = (event) => {
      clearTimeout(timeout);
      channel.port1.close();
      if (!event.data?.ok) {
        reject(new Error(event.data?.error || "Autonomi video range request failed"));
        return;
      }
      resolve(new Uint8Array(event.data.bytes));
    };
    channel.port1.onmessageerror = () => {
      fail(new Error("Autonomi video range response could not be decoded"));
    };
    channel.port1.start();
    try {
      client.postMessage(
        {
          type: "autonomi-video-range",
          sessionId,
          start,
          length,
        },
        [channel.port2],
      );
    } catch (error) {
      channel.port2.close();
      fail(error);
    }
  });
}
