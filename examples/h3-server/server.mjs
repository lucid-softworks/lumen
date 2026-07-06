// h3 (unjs) HTTP framework, served through node:http via toNodeListener.
import { createServer } from 'node:http';
import { createApp, createRouter, defineEventHandler, toNodeListener, readBody, getQuery } from 'h3';

const app = createApp();
const router = createRouter();

router.get('/', defineEventHandler(() => ({ hello: 'h3 on lumen' })));
router.get('/users/:id', defineEventHandler((event) => ({ id: event.context.params.id })));
router.get('/search', defineEventHandler((event) => ({ q: getQuery(event).q ?? null })));
router.post('/echo', defineEventHandler(async (event) => ({ youSent: await readBody(event) })));

app.use(router);

const port = Number(process.env.PORT || 3000);
createServer(toNodeListener(app)).listen(port, () => console.log(`h3 on lumen: http://localhost:${port}/`));
