// A tiny recording HTTP server for the integration ports' tests. Each test suite starts one on
// the port its demo was built against (RESEND_API_BASE, STRIPE_API_BASE, …) and scripts the
// replies. Nothing here talks to a real provider.
import { createServer } from 'node:http';

/**
 * Start a mock on `port`. `route(req)` gets `{ method, path, query, headers, body }` and returns
 * `{ status, json }`, `{ status, body, headers }` or `{ destroy: true }` (drop the connection,
 * a network error). Default 404. Every request is recorded.
 */
export async function startMock(port, route) {
  const requests = [];
  const server = createServer((req, res) => {
    const chunks = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => {
      const url = new URL(req.url, `http://127.0.0.1:${port}`);
      const entry = {
        method: req.method,
        path: url.pathname,
        query: Object.fromEntries(url.searchParams),
        headers: req.headers,
        rawHeaders: req.rawHeaders,
        body: Buffer.concat(chunks).toString('utf8'),
      };
      requests.push(entry);
      let reply;
      try {
        reply = route(entry) ?? { status: 404, json: { error: 'not found' } };
      } catch (e) {
        reply = { status: 500, json: { error: String(e) } };
      }
      if (reply.destroy) {
        req.socket.destroy();
        return;
      }
      const body = reply.json !== undefined ? JSON.stringify(reply.json) : (reply.body ?? '');
      res.writeHead(reply.status ?? 200, {
        'content-type': reply.json !== undefined ? 'application/json' : 'text/plain',
        ...(reply.headers ?? {}),
      });
      res.end(body);
    });
  });
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(port, '127.0.0.1', resolve);
  });
  return {
    requests,
    last: () => requests[requests.length - 1],
    clear: () => requests.splice(0),
    close: () => new Promise((r) => server.close(r)),
  };
}
