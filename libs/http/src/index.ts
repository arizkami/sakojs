// SPDX-License-Identifier: BSD-3-Clause

/**
 * `sako:http` -- a web-standard HTTP server on Sako's native Rust transport.
 *
 * `node:http` is the shape Node's ecosystem is written against, and Sako
 * serves it. This is the shape everything written since is: a handler takes a
 * `Request` and returns a `Response`, and the runtime owns everything in
 * between. Both run on the same overlapped-IOCP server in `sako-http`; the
 * difference is only what a handler is handed.
 *
 * ```ts
 * import { serve } from "sako:http";
 *
 * serve((request) => new Response(`hello ${new URL(request.url).pathname}`));
 * ```
 */

/**
 * What the native bindings this library is built on look like.
 *
 * The trailing `raw` flag is what separates this from `node:http`: it asks the
 * native dispatcher to call the handler with a `Request` and take a `Response`
 * back as its own return value, instead of building an IncomingMessage and a
 * ServerResponse and then asking a second time what the answer was.
 */
type NativeHandler = (request: Request, connection: ConnectionInfo) => Response | Promise<Response>;
declare const __sakoHttpListen: (
  handler: NativeHandler,
  port: number,
  raw: boolean,
) => [number, number];
declare const __sakoHttpsListen: (
  handler: NativeHandler,
  port: number,
  key: unknown,
  certificate: unknown,
  raw: boolean,
) => [number, number];
declare const __sakoHttpClose: (id: number) => void;

/** Who is asking, as far as a loopback server can tell. */
export interface ConnectionInfo {
  /** The peer's address. */
  readonly remoteAddress: string;
  /** Whether the connection is TLS. */
  readonly secure: boolean;
}

/** A handler answers one request with one response. */
export type Handler = (
  request: Request,
  connection: ConnectionInfo,
) => Response | Promise<Response>;

/** Where a running server can be reached. */
export interface ServerAddress {
  readonly hostname: string;
  readonly port: number;
  /** The origin a client would use, e.g. `http://127.0.0.1:8080`. */
  readonly url: string;
}

export interface ServeOptions {
  /** The port to bind. Zero -- the default -- takes whatever is free. */
  port?: number;
  /** The handler, when it is not passed as the first argument. */
  fetch?: Handler;
  /**
   * What to answer when a handler throws or rejects. The default logs the
   * failure and answers 500, because a handler that throws is a bug in the
   * handler and not a reason to drop the connection.
   */
  onError?: (error: unknown) => Response | Promise<Response>;
  /** Called once the port is bound. */
  onListen?: (address: ServerAddress) => void;
  /** Serve HTTPS with this key and certificate, both PEM. */
  tls?: { key: string; cert: string };
}

export interface HttpServer {
  /** Where the server is listening. */
  readonly address: ServerAddress;
  readonly port: number;
  readonly url: string;
  /** Stops accepting connections. In-flight requests still finish. */
  close(): void;
  /** Resolves once the server has been closed. */
  readonly finished: Promise<void>;
}

/**
 * Sako's native server binds the loopback interface, so this is the only
 * hostname a caller can be given honestly.
 */
const HOSTNAME = "127.0.0.1";

const defaultOnError = (error: unknown): Response => {
  // Printing beats swallowing: the alternative is a 500 whose cause exists
  // only in the process that produced it.
  console.error(error);
  return new Response("Internal Server Error", { status: 500 });
};

/**
 * Starts an HTTP server and returns once it is listening.
 *
 * The handler is called with a `Request` whose body has already been read,
 * and answers with a `Response`. Both are the platform's own types, so a
 * handler written for any other web-standard runtime runs here unchanged.
 */
export function serve(handler: Handler, options?: ServeOptions): HttpServer;
export function serve(options: ServeOptions): HttpServer;
export function serve(
  handlerOrOptions: Handler | ServeOptions,
  maybeOptions?: ServeOptions,
): HttpServer {
  const options: ServeOptions =
    typeof handlerOrOptions === "function" ? (maybeOptions ?? {}) : handlerOrOptions;
  const handler: Handler | undefined =
    typeof handlerOrOptions === "function" ? handlerOrOptions : options.fetch;
  if (typeof handler !== "function") {
    throw new TypeError("serve needs a handler, either as an argument or as options.fetch");
  }
  const onError = options.onError ?? defaultOnError;
  const secure = options.tls !== undefined;

  // Nothing here awaits, and that is the point: a handler that already has its
  // answer hands it back as the dispatch's own return value, and the response
  // goes out on the connection the request arrived on without the round trip a
  // parked connection costs. A handler that returns a promise still gets one.
  const failed = async (error: unknown): Promise<Response> => {
    try {
      const handled = await onError(error);
      if (handled instanceof Response) return handled;
    } catch {
      // An onError that throws is still an answered request.
    }
    return new Response("Internal Server Error", { status: 500 });
  };

  const settled = (response: Response): Response | Promise<Response> =>
    response instanceof Response
      ? response
      : failed(new TypeError("a handler must answer with a Response"));

  const listener = (request: Request, connection: ConnectionInfo): Response | Promise<Response> => {
    try {
      const produced = handler(request, connection);
      if (produced instanceof Response) return produced;
      return Promise.resolve(produced).then(settled, failed);
    } catch (error) {
      return failed(error);
    }
  };

  const requested = Number(options.port ?? 0);
  const binding = secure
    ? __sakoHttpsListen(listener, requested, options.tls!.key, options.tls!.cert, true)
    : __sakoHttpListen(listener, requested, true);
  const id = binding[0];
  const port = binding[1];

  let release: () => void = () => {};
  const finished = new Promise<void>((resolve) => {
    release = resolve;
  });
  let closed = false;
  const address: ServerAddress = {
    hostname: HOSTNAME,
    port,
    url: `${secure ? "https" : "http"}://${HOSTNAME}:${port}`,
  };
  const server: HttpServer = {
    address,
    port,
    url: address.url,
    close() {
      if (closed) return;
      closed = true;
      __sakoHttpClose(id);
      release();
    },
    finished,
  };
  options.onListen?.(address);
  return server;
}

export default { serve };
