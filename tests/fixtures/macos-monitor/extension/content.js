function show(text) { document.documentElement.dataset.intendantExtension=text; }
chrome.storage.local.get("fixtureText").then(value=>show(value.fixtureText||"extension ready"));
chrome.storage.onChanged.addListener((changes,area)=>{
  if(area==="local" && changes.fixtureText)show(changes.fixtureText.newValue);
});
window.addEventListener('message',event=>{
  if(event.source===window && event.data?.type==='intendant-fixture-notification')
    chrome.runtime.sendMessage({type:'fixture-notification'});
});
chrome.storage.onChanged.addListener((changes,area)=>{
  if(area==='local' && changes.notificationAccepted)
    document.documentElement.dataset.notificationAccepted=String(changes.notificationAccepted.newValue);
});
