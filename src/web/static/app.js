// tttt web viewer client.
// Talks to the tttt WebSocket endpoint using the tttt_tui protocol JSON messages.

(function () {
  "use strict";

  var term = null;
  var ws = null;
  var reconnectTimer = null;
  var credential = null; // { token } or { user, pass }
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

    term.onResize(function (dims) {
      send({ Resize: { cols: dims.cols, rows: dims.rows } });
    });
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
        '<div class="name">' + escapeHtml(s.id) + "</div>" +
        '<div class="cmd">' + escapeHtml(s.command || "") + "</div>" +
        '<div class="status' + (s.status.indexOf("running") === -1 ? " exited" : "") + '">' + escapeHtml(s.status) + "</div>";
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

  function handleServerMsg(raw) {
    var msg = JSON.parse(raw);
    if (msg.ScreenUpdate) {
      var data = msg.ScreenUpdate.screen_data;
      if (data && data.length) {
        // data is an array of bytes (JSON numbers)
        var bytes = Uint8Array.from(data);
        var dec = new TextDecoder("utf-8", { fatal: false });
        term.write(dec.decode(bytes));
      }
    } else if (msg.SessionList) {
      renderSessions(msg.SessionList);
    } else if (msg.WindowSize) {
      // Keep the browser terminal in sync with PTY dims.
      var cols = msg.WindowSize.cols, rows = msg.WindowSize.rows;
      if (term.cols !== cols || term.rows !== rows) {
        term.resize(cols, rows);
      }
    } else if (msg.Goodbye) {
      setConn("server closed", false);
      scheduleReconnect();
    }
  }

  function connect() {
    if (ws) {
      try { ws.close(); } catch (e) {}
    }
    setConn("connecting…", false);
    ws = new WebSocket(wsUrl());
    ws.binaryType = "arraybuffer";

    ws.onopen = function () {
      setConn("connected", true);
      // Announce our terminal size so the PTY is sized correctly.
      send({ Resize: { cols: term.cols, rows: term.rows } });
    };

    ws.onmessage = function (ev) {
      if (typeof ev.data === "string") {
        try { handleServerMsg(ev.data); } catch (e) { /* ignore malformed */ }
      }
    };

    ws.onclose = function () {
      setConn("disconnected", false);
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
      if (credential) {
        connect();
      } else {
        showLogin();
      }
    }, 2000);
  }

  function showLogin() {
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
        if (!info.required) {
          startWithCredential(null);
          return;
        }
        if (info.scheme === "token") {
          if (urlToken) {
            startWithCredential({ token: urlToken });
          } else {
            document.getElementById("login-token").style.display = "block";
            showLogin();
          }
        } else {
          document.getElementById("login-basic").style.display = "block";
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
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }
})();
