const filledBlock = '\u{2588}';
const emptyBlock = '\u{3000}';

var shouldAddCursor = false;
var timer;

function swap(id, a, b) {
    let el = document.getElementById(id);
    el.innerHTML = el.innerHTML.replace(a, b);
}

function removeAllBlocks(id) {
    swap(id, emptyBlock, '');
    swap(id, filledBlock, '');
}

function alternateCursor() {
    if (shouldAddCursor) {
        swap('llm-response', emptyBlock, filledBlock);
        shouldAddCursor = false;
    } else {
        swap('llm-response', filledBlock, emptyBlock);
        shouldAddCursor = true;
    }
}

function startTimer() {
    timer = setInterval(alternateCursor, 1000);
}

function stopTimer() {
    clearInterval(timer);
    timer = null;
}

document.addEventListener('htmx:sseBeforeMessage', function (e) {
    if (timer) {
        stopTimer();
    }
    removeAllBlocks('llm-response');
});

document.addEventListener('htmx:sseClose', function (e) {
    stopTimer();
    removeAllBlocks('llm-response');
    const responseEl = document.getElementById('llm-response');
    responseEl.removeAttribute('id');
});

document.addEventListener('htmx:afterRequest', function (e) {
    if (e.target.id === 'chat-input-form') {
        startTimer();
    }
});