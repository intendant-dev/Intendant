// Operates only on disposable test data; no external network or credentials.
chrome.runtime.onMessage.addListener((message,sender,reply)=>{
  if (sender.id!==chrome.runtime.id || message.type!=="store" || typeof message.text!=="string") return;
  chrome.storage.local.set({fixtureText:message.text}).then(()=>reply({stored:true}));
  return true;
});
chrome.runtime.onInstalled.addListener(()=>chrome.storage.local.set({fixtureText:"extension ready"}));
chrome.runtime.onMessage.addListener((message,sender,reply)=>{
  if(sender.id!==chrome.runtime.id) return;
  if(message.type==='fixture-notification' && sender.url?.startsWith('http://127.0.0.1:')) {
    chrome.windows.create({url:chrome.runtime.getURL('notification.html'),type:'popup',width:430,height:420,focused:true}).then(()=>reply({opened:true}));
    return true;
  }
  if(message.type==='fixture-acknowledge' && sender.url===chrome.runtime.getURL('notification.html')) {
    chrome.storage.local.set({notificationAccepted:true}).then(()=>reply({accepted:true}));
    return true;
  }
});
