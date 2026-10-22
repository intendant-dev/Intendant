document.getElementById('ack').onclick=()=>chrome.runtime.sendMessage({type:'fixture-acknowledge'}).then(reply=>{
  if(reply.accepted){document.getElementById('result').textContent='Acknowledged';window.close();}
});
