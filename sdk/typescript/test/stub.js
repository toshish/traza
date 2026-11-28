// A canned-answer HTTP server, for answers traza-server never gives.
//
// A redirect, a reverse proxy's sign-in page, a truncated body: the client
// must survive each, and the real server cannot be made to produce any of
// them. Each connection's request is read whole (headers, then Content-Length
// bytes of body) and recorded, `answer` turns it into the raw bytes to write
// back, and the connection is closed — so every answer is exactly the bytes a
// test chose, malformed ones included.

import { createServer } from "node:net";

/** A well-formed HTTP/1.1 response that closes its connection. */
export function response(status, body = "", headers = {}) {
  const payload = Buffer.from(body);
  const lines = [`HTTP/1.1 ${status} Stub`, `Content-Length: ${payload.length}`, "Connection: close"];
  for (const [name, value] of Object.entries(headers)) lines.push(`${name}: ${value}`);
  return Buffer.concat([Buffer.from(lines.join("\r\n") + "\r\n\r\n", "latin1"), payload]);
}

/** Serves `answer(request)` on an ephemeral loopback port; resolves `{ url, port, requests, close() }`. */
export async function startStub(answer) {
  const requests = [];
  const sockets = new Set();
  const server = createServer((socket) => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
    socket.on("error", () => {});
    let data = Buffer.alloc(0);
    socket.on("data", (chunk) => {
      data = Buffer.concat([data, chunk]);
      const end = data.indexOf("\r\n\r\n");
      if (end < 0) return;
      const [requestLine, ...headerLines] = data.subarray(0, end).toString("latin1").split("\r\n");
      const headers = {};
      for (const line of headerLines) {
        const colon = line.indexOf(":");
        headers[line.slice(0, colon).trim().toLowerCase()] = line.slice(colon + 1).trim();
      }
      const length = Number(headers["content-length"] ?? 0);
      if (data.length < end + 4 + length) return;
      socket.removeAllListeners("data");
      const [method, target] = requestLine.split(" ");
      const url = new URL(target, "http://stub");
      const request = {
        method,
        path: url.pathname,
        query: Object.fromEntries(url.searchParams),
        headers,
        body: data.subarray(end + 4, end + 4 + length).toString("utf8"),
      };
      requests.push(request);
      socket.end(answer(request));
    });
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address();
  return {
    port,
    url: `http://127.0.0.1:${port}`,
    requests,
    close() {
      for (const socket of sockets) socket.destroy();
      return new Promise((resolve) => server.close(resolve));
    },
  };
}
