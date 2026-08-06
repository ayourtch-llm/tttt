// tttt web viewer client.
// Talks to the tttt WebSocket endpoint using the tttt_tui protocol JSON messages.

(function () {
  "use strict";

  var term = null;
  var ws = null;
  var reconnectTimer = null;
  var credential = null; // { token } or { user, pass }
  var authRequired = false;
  var authScheme = null; // "token" | "basic" | "none"
  var connEl = document.getElementById("conn");
  var sessionListEl = document.getElementById("session-list");
  var loginOverlay = document.getElementById("login-overlay");
  var sessions = {};

  function setConn(text, ok) {
    connEl.textContent = text;
    connEl.classList.toggle("disconnected", !ok);
  }

  function b64encode(str) {
    // base64 of a UTF-8 string, browser-safe
    var bytes = new TextEncoder().encode(str);
    var bin = "";
    for (var i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]);
    return btoa(bin);
  }

  function wsUrl() {
    var proto = location.protocol === "https:" ? "wss" : "ws";
    var url = proto + "://" + location.host + "/ws";
    var params = [];
    if (credential && credential.token) params.push("token=" + encodeURIComponent(credential.token));
    if (credential && credential.user) params.push("auth=" + encodeURIComponent(b64encode(credential.user + ":" + credential.pass)));
    if (params.length) url += "?" + params.join("&");
    return url;
  }

  function initTerm() {
    term = new Terminal({
      cursorBlink: true,
      fontFamily: "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace",
      fontSize: 14,
      scrollback: 10000,
      convertEol: false,
      allowProposedApi: true,
    });
    term.open(document.getElementById("term"));

    term.onData(function (data) {
      send({ KeyInput: { bytes: Array.from(new TextEncoder().encode(data)) } });
    });

    // The PTY size is owned by the tttt TUI; the browser terminal follows
    // it via WindowSize messages, so no Resize is sent to the server.
  }

  function send(obj) {
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify(obj));
    }
  }

  function renderSessions(info) {
    sessions = {};
    sessionListEl.innerHTML = "";
    (info.sessions || []).forEach(function (s) {
      sessions[s.id] = s;
      var div = document.createElement("div");
      div.className = "session" + (info.active_id === s.id ? " active" : "");
      div.dataset.id = s.id;
      div.innerHTML =
        '<button class="close" title="Close session">&times;</button>' +
        '<div class="name">' + escapeHtml(s.id) + "</div>" +
        '<div class="cmd">' + escapeHtml(s.command || "") + "</div>" +
        '<div class="status' + (s.status.indexOf("running") === -1 ? " exited" : "") + '">' + escapeHtml(s.status) + "</div>";
      var closeBtn = div.querySelector(".close");
      closeBtn.addEventListener("click", function (ev) {
        ev.stopPropagation();
        if (confirm("Close session " + s.id + "?")) {
          send({ KillSession: { session_id: s.id } });
        }
      });
      div.addEventListener("click", function () {
        send({ SwitchSession: { session_id: s.id } });
        Array.prototype.forEach.call(sessionListEl.children, function (c) {
          c.classList.toggle("active", c.dataset.id === s.id);
        });
      });
      sessionListEl.appendChild(div);
    });
  }

  function escapeHtml(s) {
    var d = document.createElement("div");
    d.textContent = s;
    return d.innerHTML;
  }

  var screenDecoder = new TextDecoder("utf-8", { fatal: false });

  // Screen updates arrive as binary frames:
  // [cursor_row: u16 BE][cursor_col: u16 BE][raw screen bytes]
  // (raw bytes instead of a JSON number array — ~4x smaller on the wire).
  function handleScreenFrame(buf) {
    var dv = new DataView(buf);
    var row = dv.getUint16(0);
    var col = dv.getUint16(2);
    var bytes = new Uint8Array(buf, 4);
    if (bytes.length) {
      term.write(screenDecoder.decode(bytes));
    }
    // Place the cursor explicitly (1-indexed CSI H).
    term.write("\x1b[" + (row + 1) + ";" + (col + 1) + "H");
  }

  function handleServerMsg(raw) {
    var msg = JSON.parse(raw);
    // Unit variants (like Goodbye) serialize as a bare JSON string.
    if (msg === "Goodbye") {
      setConn("server closed", false);
      scheduleReconnect();
      return;
    }
    if (msg.SessionList) {
      renderSessions(msg.SessionList);
    } else if (msg.WindowSize) {
      // Keep the browser terminal in sync with PTY dims.
      var cols = msg.WindowSize.cols, rows = msg.WindowSize.rows;
      if (term.cols !== cols || term.rows !== rows) {
        term.resize(cols, rows);
      }
    } else if (msg.SessionCreated) {
      var sc = msg.SessionCreated;
      if (sc.error) {
        setConn("create failed: " + sc.error, false);
      } else if (sc.session_id) {
        term.reset();
        setConn("connected", true);
      }
    } else if (msg.SessionKilled) {
      var sk = msg.SessionKilled;
      if (!sk.success) {
        setConn("close failed: " + (sk.error || "unknown"), false);
      }
    }
  }

  function connect() {
    if (ws) {
      try { ws.close(); } catch (e) {}
    }
    setConn("connecting…", false);
    ws = new WebSocket(wsUrl());
    ws.binaryType = "arraybuffer";
    var opened = false;

    ws.onopen = function () {
      opened = true;
      setConn("connected", true);
    };

    ws.onmessage = function (ev) {
      if (typeof ev.data === "string") {
        try { handleServerMsg(ev.data); } catch (e) { /* ignore malformed */ }
      } else if (ev.data instanceof ArrayBuffer) {
        try { handleScreenFrame(ev.data); } catch (e) { /* ignore malformed */ }
      }
    };

    ws.onclose = function () {
      setConn("disconnected", false);
      if (!opened && credential) {
        // Never connected with this credential. Probe the server: if it is
        // reachable, the credential was rejected — re-show the login form
        // instead of reconnecting with the same bad credential forever.
        fetch("/api/auth")
          .then(function (r) {
            if (r.ok) {
              credential = null;
              var err = document.getElementById("login-error");
              if (err) err.textContent = "authentication failed";
              setConn("auth failed", false);
              showLogin();
            } else {
              scheduleReconnect();
            }
          })
          .catch(function () { scheduleReconnect(); });
        return;
      }
      scheduleReconnect();
    };

    ws.onerror = function () {
      setConn("connection error", false);
    };
  }

  function scheduleReconnect() {
    if (reconnectTimer) return;
    reconnectTimer = setTimeout(function () {
      reconnectTimer = null;
      if (credential || !authRequired) {
        connect();
      } else {
        showLogin();
      }
    }, 2000);
  }

  function showLogin() {
    // Make sure the form matching the server's auth scheme is visible
    // (e.g. when a URL token was rejected, no form was shown yet).
    if (authScheme === "basic") {
      document.getElementById("login-basic").style.display = "block";
      document.getElementById("login-token").style.display = "none";
    } else if (authScheme === "token") {
      document.getElementById("login-token").style.display = "block";
      document.getElementById("login-basic").style.display = "none";
    }
    loginOverlay.style.display = "flex";
  }

  function hideLogin() {
    loginOverlay.style.display = "none";
  }

  function startWithCredential(c) {
    credential = c;
    hideLogin();
    connect();
  }

  function init() {
    initTerm();

    // If a token was provided in the URL (?token=...), use it directly.
    var urlToken = new URLSearchParams(location.search).get("token");

    fetch("/api/auth")
      .then(function (r) { return r.json(); })
      .then(function (info) {
        authRequired = !!info.required;
        authScheme = info.scheme || null;
        if (!info.required) {
          startWithCredential(null);
          return;
        }
        // A URL token is worth trying under any scheme (token auth may be
        // active alongside htpasswd); if rejected, the login form appears.
        if (urlToken) {
          startWithCredential({ token: urlToken });
        } else {
          showLogin();
        }
      })
      .catch(function () {
        // Can't reach the API — just try connecting with any URL token.
        if (urlToken) startWithCredential({ token: urlToken });
        else showLogin();
      });

    document.getElementById("login-btn").addEventListener("click", function () {
      var err = document.getElementById("login-error");
      err.textContent = "";
      if (document.getElementById("login-token").style.display === "block") {
        var tok = document.getElementById("token-input").value.trim();
        if (!tok) { err.textContent = "enter a token"; return; }
        startWithCredential({ token: tok });
      } else {
        var u = document.getElementById("user-input").value.trim();
        var p = document.getElementById("pass-input").value;
        if (!u || !p) { err.textContent = "enter username and password"; return; }
        startWithCredential({ user: u, pass: p });
      }
    });

    // Create a new session: prompt for a command (empty = default shell).
    document.getElementById("new-session").addEventListener("click", function () {
      var cmd = prompt("Command to run (empty for default shell):", "");
      if (cmd === null) return; // cancelled
      var parts = splitCommand(cmd);
      send({
        CreateSession: {
          command: parts[0],
          args: parts.slice(1),
          cols: term.cols,
          rows: term.rows,
        }
      });
    });

    // Keep keyboard input in the terminal.
    document.getElementById("term").addEventListener("click", function () {
      term.focus();
    });
    term.focus();
  }

  // Minimal shell-style split supporting quotes (no escapes).
  function splitCommand(s) {
    s = (s || "").trim();
    if (!s) return [""];
    var out = [], cur = "", quote = null;
    for (var i = 0; i < s.length; i++) {
      var c = s[i];
      if (quote) {
        if (c === quote) { quote = null; }
        else { cur += c; }
      } else if (c === '"' || c === "'") {
        quote = c;
      } else if (c === " " || c === "\t") {
        if (cur) { out.push(cur); cur = ""; }
      } else {
        cur += c;
      }
    }
    if (cur) out.push(cur);
    if (!out.length) out = [""];
    return out;
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }
})();
