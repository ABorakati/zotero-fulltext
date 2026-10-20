/* sso-spike manual-mode control panel script. All logic lives in the bootstrap controller
 * passed in window.arguments[0].wrappedJSObject.ctl; this file only wires buttons. */

var sspCtl = window.arguments && window.arguments[0] && window.arguments[0].wrappedJSObject
  && window.arguments[0].wrappedJSObject.ctl;

function sspEl(id) {
  return document.getElementById(id);
}

function sspLog(line) {
  const log = sspEl('ssp-log');
  if (!log) {
    return;
  }
  log.value += line + '\n';
  log.scrollTop = log.scrollHeight;
}

function sspRender() {
  const table = sspEl('ssp-steps');
  table.textContent = '';
  const busy = sspCtl.state.busy;
  for (const step of sspCtl.steps) {
    const tr = document.createElementNS('http://www.w3.org/1999/xhtml', 'tr');
    tr.style.borderBottom = '1px solid #ccc';
    const tdLabel = document.createElementNS('http://www.w3.org/1999/xhtml', 'td');
    tdLabel.style.padding = '4px';
    tdLabel.textContent = step.label + (step.optional ? ' (optional)' : '');
    const tdBtn = document.createElementNS('http://www.w3.org/1999/xhtml', 'td');
    const btn = document.createElementNS('http://www.w3.org/1999/xhtml', 'button');
    btn.textContent = 'Start';
    btn.disabled = !!busy;
    btn.addEventListener('click', () => {
      sspCtl.start(step.id, { clearCookies: sspEl('ssp-clear').checked }).catch(e => sspLog('ERROR ' + e));
    });
    tdBtn.appendChild(btn);
    const tdStatus = document.createElementNS('http://www.w3.org/1999/xhtml', 'td');
    tdStatus.style.padding = '4px';
    tdStatus.textContent = sspCtl.state.status[step.id] || 'not run';
    tr.appendChild(tdLabel);
    tr.appendChild(tdBtn);
    tr.appendChild(tdStatus);
    table.appendChild(tr);
  }
  sspEl('ssp-finish').disabled = !(busy && sspCtl.state.awaitingUser);
  sspEl('ssp-abandon').disabled = !(busy && sspCtl.state.awaitingUser);
}

window.addEventListener('load', () => {
  if (!sspCtl) {
    sspLog('No controller passed to this window.');
    return;
  }
  sspCtl.onLog = sspLog;
  sspCtl.onChange = sspRender;
  sspEl('ssp-finish').addEventListener('click', () => {
    sspCtl.finish(sspEl('ssp-note').value).then(() => { sspEl('ssp-note').value = ''; }).catch(e => sspLog('ERROR ' + e));
  });
  sspEl('ssp-abandon').addEventListener('click', () => {
    sspCtl.abandon(sspEl('ssp-note').value).then(() => { sspEl('ssp-note').value = ''; }).catch(e => sspLog('ERROR ' + e));
  });
  sspEl('ssp-quit').addEventListener('click', () => {
    sspCtl.writeAndQuit('control-button').catch(e => sspLog('ERROR ' + e));
  });
  for (const line of sspCtl.state.logLines) {
    sspLog(line);
  }
  sspRender();
});
