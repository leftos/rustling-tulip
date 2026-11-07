import type { WebSocketLike } from "./socket";

/** The real `WebSocket`, behind the connection's small surface. */
export function openWebSocket(
  url: string,
  ctor: new (url: string) => WebSocket = WebSocket,
): WebSocketLike {
  const socket = new ctor(url);
  const like: WebSocketLike = {
    get readyState() {
      return socket.readyState;
    },
    send: (data) => {
      socket.send(data);
    },
    close: () => {
      socket.close();
    },
    onopen: null,
    onmessage: null,
    onclose: null,
    onerror: null,
  };
  socket.onopen = (event) => {
    like.onopen?.(event);
  };
  socket.onmessage = (event) => {
    like.onmessage?.(event);
  };
  socket.onclose = (event) => {
    like.onclose?.(event);
  };
  socket.onerror = (event) => {
    like.onerror?.(event);
  };
  return like;
}
