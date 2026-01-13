// State
let isPaused = false;
let autoScroll = true;
let totalCount = 0;
let cursorCount = 0;
let codexCount = 0;
let eventSource = null;

// DOM Elements
const logsDiv = document.getElementById('logs');
const totalCountEl = document.getElementById('total-count');
const cursorCountEl = document.getElementById('cursor-count');
const codexCountEl = document.getElementById('codex-count');
const statusEl = document.getElementById('connection-status');
const clearBtn = document.getElementById('clear-btn');
const pauseBtn = document.getElementById('pause-btn');
const autoScrollCheckbox = document.getElementById('auto-scroll');

// Initialize
function init() {
    connectSSE();
    setupEventListeners();
}

// Setup event listeners
function setupEventListeners() {
    clearBtn.addEventListener('click', clearLogs);
    pauseBtn.addEventListener('click', togglePause);
    autoScrollCheckbox.addEventListener('change', (e) => {
        autoScroll = e.target.checked;
        if (autoScroll) {
            scrollToBottom();
        }
    });
}

// Connect to SSE
function connectSSE() {
    if (eventSource) {
        eventSource.close();
    }

    updateStatus('reconnecting');
    eventSource = new EventSource('/logs/stream');

    eventSource.onopen = function() {
        updateStatus('connected');
        addLogEntry({
            type: 'info',
            message: 'Connected to log stream'
        });
    };

    eventSource.onmessage = function(event) {
        if (isPaused) return;

        try {
            const data = JSON.parse(event.data);
            addLogEntry(data);
        } catch (e) {
            console.error('Failed to parse log data:', e);
            addLogEntry({
                type: 'error',
                message: 'Failed to parse: ' + event.data
            });
        }
    };

    eventSource.onerror = function() {
        updateStatus('disconnected');
        addLogEntry({
            type: 'error',
            message: 'Connection lost. Reconnecting...'
        });

        // Reconnect after 3 seconds
        setTimeout(() => {
            if (eventSource.readyState === EventSource.CLOSED) {
                connectSSE();
            }
        }, 3000);
    };
}

// Add log entry
function addLogEntry(data) {
    const entry = document.createElement('div');
    entry.className = 'log-entry fade-in';

    const time = new Date().toLocaleTimeString('zh-CN', {
        hour12: false,
        hour: '2-digit',
        minute: '2-digit',
        second: '2-digit',
        fractionalSecondDigits: 3
    });

    let direction = '';
    let className = '';

    switch(data.type) {
        case 'cursor_request':
            direction = '← CURSOR';
            className = 'log-cursor';
            cursorCount++;
            cursorCountEl.textContent = cursorCount;
            break;
        case 'codex_forward':
            direction = '→ CODEX';
            className = 'log-codex';
            codexCount++;
            codexCountEl.textContent = codexCount;
            break;
        case 'codex_response':
            direction = '← CODEX';
            className = 'log-codex';
            codexCount++;
            codexCountEl.textContent = codexCount;
            break;
        case 'cursor_response':
            direction = '→ CURSOR';
            className = 'log-cursor';
            cursorCount++;
            cursorCountEl.textContent = cursorCount;
            break;
        case 'info':
            direction = 'ℹ INFO';
            className = 'log-info';
            break;
        case 'error':
            direction = '✗ ERROR';
            className = 'log-error';
            break;
        default:
            direction = '• LOG';
            className = 'log-info';
    }

    entry.className += ' ' + className;

    const timestampSpan = document.createElement('span');
    timestampSpan.className = 'timestamp';
    timestampSpan.textContent = time;

    const directionSpan = document.createElement('span');
    directionSpan.className = 'direction';
    directionSpan.textContent = direction;

    const messageSpan = document.createElement('span');
    messageSpan.className = 'message';
    messageSpan.textContent = data.message || JSON.stringify(data);

    entry.appendChild(timestampSpan);
    entry.appendChild(directionSpan);
    entry.appendChild(messageSpan);

    logsDiv.appendChild(entry);

    totalCount++;
    totalCountEl.textContent = totalCount;

    // Keep only last 500 entries
    while (logsDiv.children.length > 500) {
        logsDiv.removeChild(logsDiv.firstChild);
    }

    if (autoScroll) {
        scrollToBottom();
    }
}

// Update connection status
function updateStatus(status) {
    statusEl.className = 'stat-value status-' + status;

    switch(status) {
        case 'connected':
            statusEl.textContent = 'Connected';
            break;
        case 'disconnected':
            statusEl.textContent = 'Disconnected';
            break;
        case 'reconnecting':
            statusEl.textContent = 'Reconnecting...';
            break;
    }
}

// Clear logs
function clearLogs() {
    logsDiv.innerHTML = '';
    totalCount = 0;
    cursorCount = 0;
    codexCount = 0;
    totalCountEl.textContent = '0';
    cursorCountEl.textContent = '0';
    codexCountEl.textContent = '0';

    addLogEntry({
        type: 'info',
        message: 'Logs cleared'
    });
}

// Toggle pause
function togglePause() {
    isPaused = !isPaused;
    pauseBtn.textContent = isPaused ? 'Resume' : 'Pause';
    pauseBtn.style.background = isPaused ? '#d4d4d4' : '#0e639c';
    pauseBtn.style.color = isPaused ? '#1e1e1e' : '#fff';

    addLogEntry({
        type: 'info',
        message: isPaused ? 'Logging paused' : 'Logging resumed'
    });
}

// Scroll to bottom
function scrollToBottom() {
    logsDiv.scrollTop = logsDiv.scrollHeight;
}

// Initialize on load
window.addEventListener('DOMContentLoaded', init);

// Cleanup on unload
window.addEventListener('beforeunload', () => {
    if (eventSource) {
        eventSource.close();
    }
});
