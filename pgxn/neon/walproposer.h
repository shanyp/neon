#ifndef __NEON_WALPROPOSER_H__
#define __NEON_WALPROPOSER_H__

#include "postgres.h"
#include "access/xlogdefs.h"
#include "port.h"
#include "access/xlog_internal.h"
#include "access/transam.h"
#include "nodes/replnodes.h"
#include "utils/uuid.h"
#include "replication/walreceiver.h"

#define SK_MAGIC 0xCafeCeefu
#define SK_PROTOCOL_VERSION 2

#define MAX_SAFEKEEPERS 32
#define MAX_SEND_SIZE (XLOG_BLCKSZ * 16)	/* max size of a single* WAL
											 * message */
/*
 * In the spirit of WL_SOCKET_READABLE and others, this corresponds to no events having occurred,
 * because all WL_* events are given flags equal to some (1 << i), starting from i = 0
 */
#define WL_NO_EVENTS 0

struct WalProposerConn;			/* Defined in implementation (walprop_pg.c) */
typedef struct WalProposerConn WalProposerConn;

/* Possible return values from ReadPGAsync */
typedef enum
{
	/* The full read was successful. buf now points to the data */
	PG_ASYNC_READ_SUCCESS,

	/*
	 * The read is ongoing. Wait until the connection is read-ready, then try
	 * again.
	 */
	PG_ASYNC_READ_TRY_AGAIN,
	/* Reading failed. Check PQerrorMessage(conn) */
	PG_ASYNC_READ_FAIL,
} PGAsyncReadResult;

/* Possible return values from WritePGAsync */
typedef enum
{
	/* The write fully completed */
	PG_ASYNC_WRITE_SUCCESS,

	/*
	 * The write started, but you'll need to call PQflush some more times to
	 * finish it off. We just tried, so it's best to wait until the connection
	 * is read- or write-ready to try again.
	 *
	 * If it becomes read-ready, call PQconsumeInput and flush again. If it
	 * becomes write-ready, just call PQflush.
	 */
	PG_ASYNC_WRITE_TRY_FLUSH,
	/* Writing failed. Check PQerrorMessage(conn) */
	PG_ASYNC_WRITE_FAIL,
} PGAsyncWriteResult;

/*
 * WAL safekeeper state, which is used to wait for some event.
 *
 * States are listed here in the order that they're executed.
 *
 * Most states, upon failure, will move back to SS_OFFLINE by calls to
 * ResetConnection or ShutdownConnection.
 */
typedef enum
{
	/*
	 * Does not have an active connection and will stay that way until further
	 * notice.
	 *
	 * Moves to SS_CONNECTING_WRITE by calls to ResetConnection.
	 */
	SS_OFFLINE,

	/*
	 * Connecting states. "_READ" waits for the socket to be available for
	 * reading, "_WRITE" waits for writing. There's no difference in the code
	 * they execute when polled, but we have this distinction in order to
	 * recreate the event set in HackyRemoveWalProposerEvent.
	 *
	 * After the connection is made, "START_WAL_PUSH" query is sent.
	 */
	SS_CONNECTING_WRITE,
	SS_CONNECTING_READ,

	/*
	 * Waiting for the result of the "START_WAL_PUSH" command.
	 *
	 * After we get a successful result, sends handshake to safekeeper.
	 */
	SS_WAIT_EXEC_RESULT,

	/*
	 * Executing the receiving half of the handshake. After receiving, moves
	 * to SS_VOTING.
	 */
	SS_HANDSHAKE_RECV,

	/*
	 * Waiting to participate in voting, but a quorum hasn't yet been reached.
	 * This is an idle state - we do not expect AdvancePollState to be called.
	 *
	 * Moved externally by execution of SS_HANDSHAKE_RECV, when we received a
	 * quorum of handshakes.
	 */
	SS_VOTING,

	/*
	 * Already sent voting information, waiting to receive confirmation from
	 * the node. After receiving, moves to SS_IDLE, if the quorum isn't
	 * reached yet.
	 */
	SS_WAIT_VERDICT,

	/* Need to flush ProposerElected message. */
	SS_SEND_ELECTED_FLUSH,

	/*
	 * Waiting for quorum to send WAL. Idle state. If the socket becomes
	 * read-ready, the connection has been closed.
	 *
	 * Moves to SS_ACTIVE only by call to StartStreaming.
	 */
	SS_IDLE,

	/*
	 * Active phase, when we acquired quorum and have WAL to send or feedback
	 * to read.
	 */
	SS_ACTIVE,
} SafekeeperState;

/* Consensus logical timestamp. */
typedef uint64 term_t;

/* neon storage node id */
typedef uint64 NNodeId;

/*
 * Proposer <-> Acceptor messaging.
 */

/* Initial Proposer -> Acceptor message */
typedef struct ProposerGreeting
{
	uint64		tag;			/* message tag */
	uint32		protocolVersion;	/* proposer-safekeeper protocol version */
	uint32		pgVersion;
	pg_uuid_t	proposerId;
	uint64		systemId;		/* Postgres system identifier */
	uint8		timeline_id[16];	/* Neon timeline id */
	uint8		tenant_id[16];
	TimeLineID	timeline;
	uint32		walSegSize;
} ProposerGreeting;

typedef struct AcceptorProposerMessage
{
	uint64		tag;
} AcceptorProposerMessage;

/*
 * Acceptor -> Proposer initial response: the highest term acceptor voted for.
 */
typedef struct AcceptorGreeting
{
	AcceptorProposerMessage apm;
	term_t		term;
	NNodeId		nodeId;
} AcceptorGreeting;

/*
 * Proposer -> Acceptor vote request.
 */
typedef struct VoteRequest
{
	uint64		tag;
	term_t		term;
	pg_uuid_t	proposerId;		/* for monitoring/debugging */
} VoteRequest;

/* Element of term switching chain. */
typedef struct TermSwitchEntry
{
	term_t		term;
	XLogRecPtr	lsn;
} TermSwitchEntry;

typedef struct TermHistory
{
	uint32		n_entries;
	TermSwitchEntry *entries;
} TermHistory;

/* Vote itself, sent from safekeeper to proposer */
typedef struct VoteResponse
{
	AcceptorProposerMessage apm;
	term_t		term;
	uint64		voteGiven;

	/*
	 * Safekeeper flush_lsn (end of WAL) + history of term switches allow
	 * proposer to choose the most advanced one.
	 */
	XLogRecPtr	flushLsn;
	XLogRecPtr	truncateLsn;	/* minimal LSN which may be needed for*
								 * recovery of some safekeeper */
	TermHistory termHistory;
	XLogRecPtr	timelineStartLsn;	/* timeline globally starts at this LSN */
} VoteResponse;

/*
 * Proposer -> Acceptor message announcing proposer is elected and communicating
 * epoch history to it.
 */
typedef struct ProposerElected
{
	uint64		tag;
	term_t		term;
	/* proposer will send since this point */
	XLogRecPtr	startStreamingAt;
	/* history of term switches up to this proposer */
	TermHistory *termHistory;
	/* timeline globally starts at this LSN */
	XLogRecPtr	timelineStartLsn;
} ProposerElected;

/*
 * Header of request with WAL message sent from proposer to safekeeper.
 */
typedef struct AppendRequestHeader
{
	uint64		tag;
	term_t		term;			/* term of the proposer */

	/*
	 * LSN since which current proposer appends WAL (begin_lsn of its first
	 * record); determines epoch switch point.
	 */
	XLogRecPtr	epochStartLsn;
	XLogRecPtr	beginLsn;		/* start position of message in WAL */
	XLogRecPtr	endLsn;			/* end position of message in WAL */
	XLogRecPtr	commitLsn;		/* LSN committed by quorum of safekeepers */

	/*
	 * minimal LSN which may be needed for recovery of some safekeeper (end
	 * lsn + 1 of last chunk streamed to everyone)
	 */
	XLogRecPtr	truncateLsn;
	pg_uuid_t	proposerId;		/* for monitoring/debugging */
} AppendRequestHeader;

/*
 * Hot standby feedback received from replica
 */
typedef struct HotStandbyFeedback
{
	TimestampTz ts;
	FullTransactionId xmin;
	FullTransactionId catalog_xmin;
} HotStandbyFeedback;

typedef struct PageserverFeedback
{
	/* current size of the timeline on pageserver */
	uint64		currentClusterSize;
	/* standby_status_update fields that safekeeper received from pageserver */
	XLogRecPtr	last_received_lsn;
	XLogRecPtr	disk_consistent_lsn;
	XLogRecPtr	remote_consistent_lsn;
	TimestampTz replytime;
} PageserverFeedback;

typedef struct WalproposerShmemState
{
	slock_t		mutex;
	PageserverFeedback feedback;
	term_t		mineLastElectedTerm;
	pg_atomic_uint64 backpressureThrottlingTime;
} WalproposerShmemState;

/*
 * Report safekeeper state to proposer
 */
typedef struct AppendResponse
{
	AcceptorProposerMessage apm;

	/*
	 * Current term of the safekeeper; if it is higher than proposer's, the
	 * compute is out of date.
	 */
	term_t		term;
	/* TODO: add comment */
	XLogRecPtr	flushLsn;
	/* Safekeeper reports back his awareness about which WAL is committed, as */
	/* this is a criterion for walproposer --sync mode exit */
	XLogRecPtr	commitLsn;
	HotStandbyFeedback hs;
	/* Feedback received from pageserver includes standby_status_update fields */
	/* and custom neon feedback. */
	/* This part of the message is extensible. */
	PageserverFeedback rf;
} AppendResponse;

/*  PageserverFeedback is extensible part of the message that is parsed separately */
/*  Other fields are fixed part */
#define APPENDRESPONSE_FIXEDPART_SIZE offsetof(AppendResponse, rf)

struct WalProposer;
typedef struct WalProposer WalProposer;

/*
 * Descriptor of safekeeper
 */
typedef struct Safekeeper
{
	WalProposer *wp;

	char const *host;
	char const *port;

	/*
	 * connection string for connecting/reconnecting.
	 *
	 * May contain private information like password and should not be logged.
	 */
	char		conninfo[MAXCONNINFO];

	/*
	 * Temporary buffer for the message being sent to the safekeeper.
	 */
	StringInfoData outbuf;

	/*
	 * Streaming will start here; must be record boundary.
	 */
	XLogRecPtr	startStreamingAt;

	bool		flushWrite;		/* set to true if we need to call AsyncFlush,*
								 * to flush pending messages */
	XLogRecPtr	streamingAt;	/* current streaming position */
	AppendRequestHeader appendRequest;	/* request for sending to safekeeper */

	SafekeeperState state;		/* safekeeper state machine state */
	TimestampTz latestMsgReceivedAt;	/* when latest msg is received */
	AcceptorGreeting greetResponse; /* acceptor greeting */
	VoteResponse voteResponse;	/* the vote */
	AppendResponse appendResponse;	/* feedback for master */


	/* postgres-specific fields */
	#ifndef WALPROPOSER_LIB
	/*
	 * postgres protocol connection to the WAL acceptor
	 *
	 * Equals NULL only when state = SS_OFFLINE. Nonblocking is set once we
	 * reach SS_ACTIVE; not before.
	 */
	WalProposerConn *conn;

	/*
	 * WAL reader, allocated for each safekeeper.
	 */
	XLogReaderState *xlogreader;

	/*
	 * Position in wait event set. Equal to -1 if no event
	 */
	int			eventPos;
	#endif


	/* WalProposer library specifics */
	#ifdef WALPROPOSER_LIB
	/*
	 * Buffer for incoming messages. Usually Rust vector is stored here.
	 * Caller is responsible for freeing the buffer.
	 */
	StringInfoData inbuf;
	#endif
} Safekeeper;

/* Re-exported PostgresPollingStatusType */
typedef enum
{
	WP_CONN_POLLING_FAILED = 0,
	WP_CONN_POLLING_READING,
	WP_CONN_POLLING_WRITING,
	WP_CONN_POLLING_OK,

	/*
	 * 'libpq-fe.h' still has PGRES_POLLING_ACTIVE, but says it's unused.
	 * We've removed it here to avoid clutter.
	 */
} WalProposerConnectPollStatusType;

/* Re-exported and modified ExecStatusType */
typedef enum
{
	/* We received a single CopyBoth result */
	WP_EXEC_SUCCESS_COPYBOTH,

	/*
	 * Any success result other than a single CopyBoth was received. The
	 * specifics of the result were already logged, but it may be useful to
	 * provide an error message indicating which safekeeper messed up.
	 *
	 * Do not expect PQerrorMessage to be appropriately set.
	 */
	WP_EXEC_UNEXPECTED_SUCCESS,

	/*
	 * No result available at this time. Wait until read-ready, then call
	 * again. Internally, this is returned when PQisBusy indicates that
	 * PQgetResult would block.
	 */
	WP_EXEC_NEEDS_INPUT,
	/* Catch-all failure. Check PQerrorMessage. */
	WP_EXEC_FAILED,
} WalProposerExecStatusType;

/* Re-exported ConnStatusType */
typedef enum
{
	WP_CONNECTION_OK,
	WP_CONNECTION_BAD,

	/*
	 * The original ConnStatusType has many more tags, but requests that they
	 * not be relied upon (except for displaying to the user). We don't need
	 * that extra functionality, so we collect them into a single tag here.
	 */
	WP_CONNECTION_IN_PROGRESS,
} WalProposerConnStatusType;

/*
 * Collection of hooks for walproposer, to call postgres functions,
 * read WAL and send it over the network.
 */
typedef struct walproposer_api
{
	/*
	 * Get WalproposerShmemState. This is used to store information about last
	 * elected term.
	 */
	WalproposerShmemState *(*get_shmem_state) (WalProposer *wp);

	/*
	 * Start receiving notifications about new WAL. This is an infinite loop
	 * which calls WalProposerBroadcast() and WalProposerPoll() to send the
	 * WAL.
	 */
	void		(*start_streaming) (WalProposer *wp, XLogRecPtr startpos);

	/* Get pointer to the latest available WAL. */
	XLogRecPtr	(*get_flush_rec_ptr) (WalProposer *wp);

	/* Get current time. */
	TimestampTz (*get_current_timestamp) (WalProposer *wp);

	/* Current error message, aka PQerrorMessage. */
	char	   *(*conn_error_message) (Safekeeper *sk);

	/* Connection status, aka PQstatus. */
	WalProposerConnStatusType (*conn_status) (Safekeeper *sk);

	/* Start the connection, aka PQconnectStart. */
	void (*conn_connect_start) (Safekeeper *sk);

	/* Poll an asynchronous connection, aka PQconnectPoll. */
	WalProposerConnectPollStatusType (*conn_connect_poll) (Safekeeper *sk);

	/* Send a blocking SQL query, aka PQsendQuery. */
	bool		(*conn_send_query) (Safekeeper *sk, char *query);

	/* Read the query result, aka PQgetResult. */
	WalProposerExecStatusType (*conn_get_query_result) (Safekeeper *sk);

	/* Flush buffer to the network, aka PQflush. */
	int			(*conn_flush) (Safekeeper *sk);

	/* Close the connection, aka PQfinish. */
	void		(*conn_finish) (Safekeeper *sk);

	/*
	 * Try to read CopyData message from the safekeeper, aka PQgetCopyData. 
	 *
	 * On success, the data is placed in *buf. It is valid until the next call
	 * to this function.
	 */
	PGAsyncReadResult (*conn_async_read) (Safekeeper *sk, char **buf, int *amount);

	/* Try to write CopyData message, aka PQputCopyData. */
	PGAsyncWriteResult (*conn_async_write) (Safekeeper *sk, void const *buf, size_t size);

	/* Blocking CopyData write, aka PQputCopyData + PQflush. */
	bool		(*conn_blocking_write) (Safekeeper *sk, void const *buf, size_t size);

	/* Download WAL from startpos to endpos and make it available locally. */
	bool		(*recovery_download) (Safekeeper *sk, TimeLineID timeline, XLogRecPtr startpos, XLogRecPtr endpos);

	/* Read WAL from disk to buf. */
	void		(*wal_read) (Safekeeper *sk, char *buf, XLogRecPtr startptr, Size count);

	/* Allocate WAL reader. */
	void (*wal_reader_allocate) (Safekeeper *sk);

	/* Deallocate event set. */
	void		(*free_event_set) (WalProposer *wp);

	/* Initialize event set. */
	void		(*init_event_set) (WalProposer *wp);

	/* Update events for an existing safekeeper connection. */
	void		(*update_event_set) (Safekeeper *sk, uint32 events);

	/* Add a new safekeeper connection to the event set. */
	void		(*add_safekeeper_event_set) (Safekeeper *sk, uint32 events);

	/*
	 * Wait until some event happens: - timeout is reached - socket event for
	 * safekeeper connection - new WAL is available
	 *
	 * Returns 0 if timeout is reached, 1 if some event happened. Updates
	 * events mask to indicate events and sets sk to the safekeeper which has
	 * an event.
	 */
	int			(*wait_event_set) (WalProposer *wp, long timeout, Safekeeper **sk, uint32 *events);

	/* Read random bytes. */
	bool		(*strong_random) (WalProposer *wp, void *buf, size_t len);

	/*
	 * Get a basebackup LSN. Used to cross-validate with the latest available
	 * LSN on the safekeepers.
	 */
	XLogRecPtr	(*get_redo_start_lsn) (WalProposer *wp);

	/*
	 * Finish sync safekeepers with the given LSN. This function should not
	 * return and should exit the program.
	 */
	void		(*finish_sync_safekeepers) (WalProposer *wp, XLogRecPtr lsn);

	/*
	 * Called after every new message from the safekeeper. Used to propagate
	 * backpressure feedback and to confirm WAL persistence (has been commited
	 * on the quorum of safekeepers).
	 */
	void		(*process_safekeeper_feedback) (WalProposer *wp, XLogRecPtr commitLsn);

	/*
	 * Called on peer_horizon_lsn updates. Used to advance replication slot
	 * and to free up disk space by deleting unnecessary WAL.
	 */
	void		(*confirm_wal_streamed) (WalProposer *wp, XLogRecPtr lsn);

	/*
	 * Write a log message to the internal log processor. This is used only
	 * when walproposer is compiled as a library. Otherwise, all logging is
	 * handled by elog().
	 */
	void		(*log_internal) (WalProposer *wp, int level, const char *line);

	/*
	 * Called right after the proposer was elected, but before it started
	 * recovery and sent ProposerElected message to the safekeepers.
	 * 
	 * Used by logical replication to update truncateLsn.
	 */
	void		(*after_election) (WalProposer *wp);
} walproposer_api;

/*
 * Configuration of the WAL proposer.
 */
typedef struct WalProposerConfig
{
	/* hex-encoded TenantId cstr */
	char	   *neon_tenant;

	/* hex-encoded TimelineId cstr */
	char	   *neon_timeline;

	/*
	 * Comma-separated list of safekeepers, in the following format:
	 * host1:port1,host2:port2,host3:port3
	 *
	 * This cstr should be editable.
	 */
	char	   *safekeepers_list;

	/*
	 * WalProposer reconnects to offline safekeepers once in this interval.
	 * Time is in milliseconds.
	 */
	int			safekeeper_reconnect_timeout;

	/*
	 * WalProposer terminates the connection if it doesn't receive any message
	 * from the safekeeper in this interval. Time is in milliseconds.
	 */
	int			safekeeper_connection_timeout;

	/*
	 * WAL segment size. Will be passed to safekeepers in greet request. Also
	 * used to detect page headers.
	 */
	int			wal_segment_size;

	/*
	 * If safekeeper was started in sync mode, walproposer will not subscribe
	 * for new WAL and will exit when quorum of safekeepers will be synced to
	 * the latest available LSN.
	 */
	bool		syncSafekeepers;

	/* Will be passed to safekeepers in greet request. */
	uint64		systemId;

	/* Will be passed to safekeepers in greet request. */
	TimeLineID  pgTimeline;

#ifdef WALPROPOSER_LIB
	void *callback_data;
#endif
} WalProposerConfig;


/*
 * WAL proposer state.
 */
typedef struct WalProposer
{
	WalProposerConfig *config;
	int			n_safekeepers;

	/* (n_safekeepers / 2) + 1 */
	int			quorum;

	Safekeeper	safekeeper[MAX_SAFEKEEPERS];

	/* WAL has been generated up to this point */
	XLogRecPtr	availableLsn;

	/* last commitLsn broadcasted to safekeepers */
	XLogRecPtr	lastSentCommitLsn;

	ProposerGreeting greetRequest;

	/* Vote request for safekeeper */
	VoteRequest voteRequest;

	/*
	 * Minimal LSN which may be needed for recovery of some safekeeper,
	 * record-aligned (first record which might not yet received by someone).
	 */
	XLogRecPtr	truncateLsn;

	/*
	 * Term of the proposer. We want our term to be highest and unique, so we
	 * collect terms from safekeepers quorum, choose max and +1. After that
	 * our term is fixed and must not change. If we observe that some
	 * safekeeper has higher term, it means that we have another running
	 * compute, so we must stop immediately.
	 */
	term_t		propTerm;

	/* term history of the proposer */
	TermHistory propTermHistory;

	/* epoch start lsn of the proposer */
	XLogRecPtr	propEpochStartLsn;

	/* Most advanced acceptor epoch */
	term_t		donorEpoch;

	/* Most advanced acceptor */
	int			donor;

	/* timeline globally starts at this LSN */
	XLogRecPtr	timelineStartLsn;

	/* number of votes collected from safekeepers */
	int			n_votes;

	/* number of successful connections over the lifetime of walproposer */
	int			n_connected;

	/*
	 * Timestamp of the last reconnection attempt. Related to
	 * config->safekeeper_reconnect_timeout
	 */
	TimestampTz last_reconnect_attempt;

	walproposer_api api;
} WalProposer;

extern WalProposer *WalProposerCreate(WalProposerConfig *config, walproposer_api api);
extern void WalProposerStart(WalProposer *wp);
extern void WalProposerBroadcast(WalProposer *wp, XLogRecPtr startpos, XLogRecPtr endpos);
extern void WalProposerPoll(WalProposer *wp);
extern void WalProposerFree(WalProposer *wp);


#define WPEVENT		1337	/* special log level for walproposer internal events */

#ifdef WALPROPOSER_LIB
void WalProposerLibLog(WalProposer *wp, int elevel, char *fmt, ...);
#define walprop_log(elevel, ...) WalProposerLibLog(wp, elevel, __VA_ARGS__)
#else
#define walprop_log(elevel, ...) elog(elevel, __VA_ARGS__)
#endif

#endif							/* __NEON_WALPROPOSER_H__ */
