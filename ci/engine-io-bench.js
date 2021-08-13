const socket = require('engine.io-client')('ws://localhost:4201', {
  transport: ['polling']
});
console.log("started");
socket.on('open', () => {
  socket.on('message', (data) => {
    console.log(data);
  });
  socket.on('close', () => {
    console.log("closed");
  });
  console.log("open");
  var iterations = 100000;
  for (var i=0;i<iterations;i++) {
    var t0 = performance.now();
    socket.send("hello world", null, () => {
      var t1 = performance.now();
      console.log((t1-t0)/iterations);
    });
    socket.flush();
  }
  console.log("done.");
  socket.close();
});
