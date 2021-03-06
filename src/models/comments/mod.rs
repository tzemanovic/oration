use bincode::{deserialize, serialize};
use bloomfilter::Bloom;
use chrono::{DateTime, NaiveDateTime, Utc};
use crypto::digest::Digest;
use crypto::sha2::Sha224;
use diesel;
use diesel::expression::dsl::sql;
use diesel::prelude::*;
use diesel::sql_types::Integer;
use diesel::sqlite::SqliteConnection;
use itertools::join;
use petgraph::graphmap::DiGraphMap;
use std::str;

use data::{AuthHash, FormEdit, FormInput};
use errors::*;
use schema::comments;

#[derive(Queryable, Debug)]
/// Queryable reference to the comments table.
pub struct Comment {
    /// Primary key.
    id: i32,
    /// Reference to Thread.
    tid: i32, //TODO: Diesel parsed this as a bool. Write up a new issue.
    /// Parent comment.
    parent: Option<i32>,
    /// Timestamp of creation.
    created: NaiveDateTime,
    /// Date modified it that's happened.
    modified: Option<NaiveDateTime>,
    /// If the comment is live or under review.
    mode: i32,
    /// Remote IP.
    remote_addr: Option<String>,
    /// Actual comment.
    text: String,
    /// Commentors author if given.
    author: Option<String>,
    /// Commentors email address if given.
    email: Option<String>,
    /// Commentors website if given.
    website: Option<String>,
    /// Commentors idenifier hash.
    hash: String,
    /// Number of likes a comment has recieved.
    likes: Option<i32>, //TODO: I know the tables like i32s, but these really should be unsigned
    /// Number of dislikes a comment has recieved.
    dislikes: Option<i32>,
    /// Who are the voters on this comment.
    voters: Option<Vec<u8>>,
}

#[derive(Insertable, Debug)]
#[table_name = "comments"]
/// Insertable reference to the comments table.
struct NewComment<'c> {
    /// Reference to Thread.
    tid: i32,
    /// Parent comment.
    parent: Option<i32>,
    /// Timestamp of creation.
    created: NaiveDateTime,
    /// Date modified it that's happened.
    modified: Option<NaiveDateTime>,
    /// If the comment is live or under review. By default an active comment has mode 0.
    /// If the admin has reviews turned on, all new comments will be flagged as mode 1, or
    /// will be set with a default mode 0 if this feature is not enabled. A comment with mode
    /// 2 indicates this comment is `deleted`, although it contains responses below it. The
    /// deleted comment with therefore be handled differently.
    mode: i32,
    /// Remote IP.
    remote_addr: Option<&'c str>,
    /// Actual comment.
    text: &'c str,
    /// Commentors author if given.
    author: Option<String>,
    /// Commentors email address if given.
    email: Option<String>,
    /// Commentors website if given.
    website: Option<String>,
    /// Sha224 hash to identify commentor.
    hash: String,
    /// Number of likes a comment has recieved.
    likes: Option<i32>,
    /// Number of dislikes a comment has recieved.
    dislikes: Option<i32>,
    /// Who are the voters on this comment.
    voters: Option<Vec<u8>>,
}

impl Comment {
    /// Returns the number of comments for a given post denoted via the `path` variable.
    pub fn count(conn: &SqliteConnection, path: &str) -> Result<i64> {
        use schema::threads;

        let comment_count = comments::table
            .inner_join(threads::table)
            .filter(threads::uri.eq(path))
            .count()
            .first(conn)
            .chain_err(|| ErrorKind::DBRead)?;

        Ok(comment_count)
    }

    /// Stores a new comment into the database.
    pub fn insert<'c>(
        conn: &SqliteConnection,
        tid: i32,
        form: &FormInput,
        ip_addr: &'c str,
        nesting_limit: u32,
    ) -> Result<InsertedComment> {
        let time = Utc::now().naive_utc();

        let ip = if ip_addr.is_empty() {
            None //TODO: I wonder if this is ever true?
        } else {
            Some(ip_addr)
        };

        let parent_id = nesting_check(conn, form.parent, nesting_limit)?;
        let hash = gen_hash(&form.name, &form.email, &form.url, Some(ip_addr));

        let c = NewComment {
            tid,
            parent: parent_id,
            created: time,
            modified: None,
            mode: 0,
            remote_addr: ip,
            text: &form.comment,
            author: form.name.clone(),
            email: form.email.clone(),
            website: form.url.clone(),
            hash,
            likes: None,
            dislikes: None,
            voters: None,
        };

        let result = diesel::insert_into(comments::table)
            .values(&c)
            .execute(conn)
            .is_ok();
        if result {
            //Return a NestedComment formated result of this entry to the front end
            let comment_id = comments::table
                .select(comments::id)
                .order(comments::id.desc())
                .first::<i32>(conn)
                .chain_err(|| ErrorKind::DBRead)?;
            let comment = PrintedComment::get(conn, comment_id)?;
            Ok(InsertedComment::new(&comment))
        } else {
            Err(ErrorKind::DBInsert.into())
        }
    }

    /// Deletes a comment if there is no children, marks as deleted if there are children.
    pub fn delete(conn: &SqliteConnection, id: i32) -> Result<()> {
        let children_count = comments::table
            .filter(comments::parent.eq(id))
            .count()
            .first::<i64>(conn)
            .chain_err(|| ErrorKind::DBRead)?;
        if children_count == 0 {
            //We can safely delete this comment entirely
            diesel::delete(comments::table.filter(comments::id.eq(id)))
                .execute(conn)
                .chain_err(|| ErrorKind::DBRead)?;
        } else {
            //This comment must be flagged as deleted instead
            let target = comments::table.filter(comments::id.eq(id));
            diesel::update(target)
                .set(&ModeDelete {
                    mode: 2,
                    remote_addr: None,
                    text: String::new(),
                    author: None,
                    email: None,
                    website: None,
                    hash: String::new(),
                    likes: None,
                    dislikes: None,
                    voters: None,
                })
                .execute(conn)
                .chain_err(|| ErrorKind::DBRead)?;
        }

        //Deleted comments may have had children before, but this request may have just
        //removed the last one of them. In that case we can completely remove the node

        // We can't chain the IN clause here, so we return it first
        // https://github.com/diesel-rs/diesel/issues/1369#issuecomment-351100511
        let child = comments::table
            .select(comments::parent)
            .filter(comments::parent.is_not_null())
            .load::<Option<i32>>(conn)
            .chain_err(|| ErrorKind::DBRead)?;
        // child is now a Vec<Option<i32>>, where all of the Options must be Some. Let's unwrap them.
        let child_unwrapped: Vec<i32> = child.into_iter().map(|c| c.unwrap_or_else(|| 0)).collect();
        let target = comments::table
            .filter(comments::mode.eq(2))
            .filter(comments::id.ne_all(child_unwrapped));
        diesel::delete(target)
            .execute(conn)
            .chain_err(|| ErrorKind::DBRead)?;

        Ok(())
    }

    /// Updates a comment.
    pub fn update<'c>(
        conn: &SqliteConnection,
        id: i32,
        data: &FormEdit,
        ip_addr: &'c str,
    ) -> Result<CommentEdits> {
        let target = comments::table.filter(comments::id.eq(id));
        let hash = gen_hash(&data.name, &data.email, &data.url, Some(ip_addr));
        let time = Utc::now().naive_utc();
        diesel::update(target)
            .set((
                comments::text.eq(data.comment.to_owned()),
                comments::author.eq(data.name.to_owned()),
                comments::email.eq(data.email.to_owned()),
                comments::website.eq(data.url.to_owned()),
                comments::hash.eq(hash),
                comments::modified.eq(Some(time)),
            ))
            .execute(conn)
            .chain_err(|| ErrorKind::DBRead)?;
        let comment = PrintedComment::get(conn, id)?;
        Ok(CommentEdits::new(&comment))
    }

    /// Called from the like and dislike functions and updates the vote tally for the
    /// given comment, provided the user is able to vote on this comment.
    /// We use the user's IP address here rather than the hash to ratelimit voting from
    /// the same IP by changing user details or spamming hash headers.
    pub fn vote<'c>(
        conn: &SqliteConnection,
        id: i32,
        ip_addr: &'c str,
        upvote: bool,
    ) -> Result<()> {
        let voters_blob = comments::table
            .select(comments::voters)
            .filter(comments::id.eq(id))
            .first::<Option<Vec<u8>>>(conn)
            .chain_err(|| ErrorKind::DBRead)?;

        let mut can_vote = true;
        if let Some(voters) = voters_blob {
            let blob: VotersBlob = deserialize(&voters).unwrap();
            let mut bloom =
                Bloom::from_existing(&blob.bitmap, blob.bits, blob.hashes, blob.sip_keys);
            if bloom.check_and_set(ip_addr) {
                //The IP is already in the database, so the user has already voted
                //for the moment, this means once a vote is cast, we don't allow a user to change
                //their vote
                can_vote = false;
            } else {
                //The IP is not in the database, the updated filter needs to be stored
                blob.store(conn, id)?;
            }
        } else {
            // New bloomfilter with 95% success rate, give it space for 150 votes by default
            let mut bloom = Bloom::new_for_fp_rate(150, 0.05);
            // Add the current user's IP to the filter
            bloom.set(ip_addr);

            let blob = VotersBlob::new(&bloom);
            blob.store(conn, id)?;
        }
        if can_vote {
            let target = comments::table.filter(comments::id.eq(id));
            // It would be nice to extract the `set` line here, but I can't seem to figure out how
            if upvote {
                diesel::update(target)
                    .set(comments::likes.eq(comments::likes + 1))
                    .execute(conn)
                    .chain_err(|| ErrorKind::DBRead)?;
            } else {
                diesel::update(target)
                    .set(comments::dislikes.eq(comments::dislikes + 1))
                    .execute(conn)
                    .chain_err(|| ErrorKind::DBRead)?;
            };
            Ok(())
        } else {
            Err(ErrorKind::AlreadyVoted.into())
        }
    }
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
/// Bloom encoding for voters. Currently more a testing phase than final product.
struct VotersBlob {
    /// Probabilistic matrix.
    bitmap: Vec<u8>,
    /// Number of bits in filter.
    bits: u64,
    /// All hashes in the filter.
    hashes: u32,
    /// Required sip keys.
    sip_keys: [(u64, u64); 2],
}

impl VotersBlob {
    /// Generate a voters struct.
    fn new(bloom: &Bloom) -> VotersBlob {
        VotersBlob {
            bitmap: bloom.bitmap(),
            bits: bloom.number_of_bits(),
            hashes: bloom.number_of_hash_functions(),
            sip_keys: bloom.sip_keys(),
        }
    }

    /// Encode the bloom filter and store it in the database.
    fn store(self, conn: &SqliteConnection, id: i32) -> Result<()> {
        let blob_encoded: Vec<u8> = serialize(&self).chain_err(|| ErrorKind::Serialize)?;

        let target = comments::table.filter(comments::id.eq(id));
        diesel::update(target)
            .set(comments::voters.eq(blob_encoded))
            .execute(conn)
            .chain_err(|| ErrorKind::DBRead)?;

        Ok(())
    }
}

#[derive(AsChangeset)]
#[table_name = "comments"]
#[changeset_options(treat_none_as_null = "true")]
/// Changes required when we must use the flagged delete option.
struct ModeDelete {
    /// If the comment is live or under review.
    mode: i32,
    /// Remote IP.
    remote_addr: Option<String>,
    /// Actual comment.
    text: String,
    /// Commentors author if given.
    author: Option<String>,
    /// Commentors email address if given.
    email: Option<String>,
    /// Commentors website if given.
    website: Option<String>,
    /// Commentors idenifier hash.
    hash: String,
    /// Number of likes a comment has recieved.
    likes: Option<i32>,
    /// Number of dislikes a comment has recieved.
    dislikes: Option<i32>,
    /// Who are the voters on this comment.
    voters: Option<Vec<u8>>,
}

/// Checks if this comment is nested too deep based on the configuration file value.
/// If so, don't allow this to happen and just post as a reply to the previous parent.
fn nesting_check(
    conn: &SqliteConnection,
    parent: Option<i32>,
    nesting_limit: u32,
) -> Result<Option<i32>> {
    match parent {
        Some(pid) => {
            //NOTE: UNION ALL and WITH RECURSIVE are currently not supported by diesel
            //https://github.com/diesel-rs/diesel/issues/33
            //https://github.com/diesel-rs/diesel/issues/356
            //So this is implemented in native SQL for the moment
            //TODO: since SqlLiteral#bind is depreciated, we should be using `sql_query`
            //here. However: we're building a virtual table and pulling a count from it.
            //Diesel for the moment AFAIK is not a happy camper about this.
            let mut query = String::from(
                "WITH RECURSIVE node_ancestors(node_id, parent_id) AS (
                    SELECT id, id FROM comments WHERE id = ",
            );
            query.push_str(&pid.to_string());
            query.push_str(
                "
                UNION ALL
                    SELECT na.node_id, comments.parent
                    FROM node_ancestors AS na, comments
                    WHERE comments.id = na.parent_id AND comments.parent IS NOT NULL
                )
                SELECT COUNT(parent_id) AS depth FROM node_ancestors GROUP BY node_id;",
            );
            let parent_depth: Vec<i32> = sql::<Integer>(&query)
                .load(conn)
                .chain_err(|| ErrorKind::DBRead)?;

            if parent_depth.is_empty() || parent_depth[0] <= nesting_limit as i32 {
                //We're fine to nest
                Ok(Some(pid as i32))
            } else {
                //We've hit the limit, reply to the current parent's parent only.
                let parents_parent: Option<i32> = comments::table
                    .select(comments::parent)
                    .filter(comments::id.eq(pid))
                    .first(conn)
                    .chain_err(|| ErrorKind::DBRead)?;
                Ok(parents_parent)
            }
        }
        None => Ok(None), //We don't need to worry about this check for new comments
    }
}

/// Generates a Sha224 hash of author details.
/// If none are set, then the possiblity of using a clients' IP address is available.
pub fn gen_hash(
    author: &Option<String>,
    email: &Option<String>,
    url: &Option<String>,
    ip_addr: Option<&str>,
) -> String {
    // Generate users sha224 hash
    let mut hasher = Sha224::new();
    //TODO: This section is pretty nasty at the moment.
    //There has to be a better way to organise this.
    let is_data = {
        //Check if any of the optional values have data in them
        let user = [&author, &email, &url];
        user.into_iter().any(|&v| v.is_some())
    };
    if is_data {
        //Generate a set of data to hash
        let mut data: Vec<String> = Vec::new();
        if let Some(val) = author.clone() {
            data.push(val)
        };
        if let Some(val) = email.clone() {
            data.push(val)
        };
        if let Some(val) = url.clone() {
            data.push(val)
        };
        //Join with 'b' since it gives the author a nice identicon
        hasher.input_str(&join(data.iter(), "b"));
    } else if let Some(ip) = ip_addr {
        //If we have no data but an ip, hash the ip, otherwise return an empty string
        hasher.input_str(ip);
    } else {
        return String::default();
    }
    hasher.result_str()
}

/// We only want users to be able to edit their comments if they accidentally produced a
/// spelling mistake or somesuch. This method removes that ablility after some `offset` time.
pub fn update_authorised(
    conn: &SqliteConnection,
    hash: &AuthHash,
    id: i32,
    offset: f32,
) -> Result<()> {
    let (stored_hash, created, modified) = comments::table
        .select((comments::hash, comments::created, comments::modified))
        .filter(comments::id.eq(id))
        .first::<(String, NaiveDateTime, Option<NaiveDateTime>)>(conn)
        .chain_err(|| ErrorKind::DBRead)?;

    // Check we haven't timed out
    let now_timestamp = Utc::now().naive_utc().timestamp();

    let updated_timestamp = {
        if let Some(mod_time) = modified {
            mod_time.timestamp()
        } else {
            created.timestamp()
        }
    };

    if hash.matches(&stored_hash) & (now_timestamp - updated_timestamp < (offset as i64)) {
        Ok(())
    } else {
        Err(ErrorKind::Unauthorized.into())
    }
}

#[derive(Serialize, Queryable, Debug)]
/// Subset of the comments table which is to be sent to the frontend.
struct PrintedComment {
    /// Primary key.
    id: i32,
    /// Parent comment.
    parent: Option<i32>,
    /// Actual comment.
    text: String,
    /// Commentors author if given.
    author: Option<String>,
    /// Commentors email address if given.
    email: Option<String>,
    /// Commentors website if given.
    url: Option<String>,
    /// Commentors indentifier.
    hash: String,
    /// Timestamp of creation.
    created: NaiveDateTime,
    /// Number of likes a comment has recieved.
    likes: Option<i32>,
    /// Number of dislikes a comment has recieved.
    dislikes: Option<i32>,
}

impl PrintedComment {
    /// Returns a list of all comments for a given post denoted via the `path` variable.
    fn list(conn: &SqliteConnection, path: &str) -> Result<Vec<PrintedComment>> {
        use schema::threads;

        let comments: Vec<PrintedComment> = comments::table
            .select((
                comments::id,
                comments::parent,
                comments::text,
                comments::author,
                comments::email,
                comments::website,
                comments::hash,
                comments::created,
                comments::likes,
                comments::dislikes,
            ))
            .inner_join(threads::table)
            .filter(
                threads::uri
                    .eq(path)
                    .and(comments::mode.eq(0).or(comments::mode.eq(2))),
            )
            .load(conn)
            .chain_err(|| ErrorKind::DBRead)?;
        Ok(comments)
    }

    /// Returns a comment based on its' unique ID.
    pub fn get(conn: &SqliteConnection, id: i32) -> Result<PrintedComment> {
        let comment: PrintedComment = comments::table
            .select((
                comments::id,
                comments::parent,
                comments::text,
                comments::author,
                comments::email,
                comments::website,
                comments::hash,
                comments::created,
                comments::likes,
                comments::dislikes,
            ))
            .filter(comments::id.eq(id))
            .first(conn)
            .chain_err(|| ErrorKind::DBRead)?;
        Ok(comment)
    }
}

#[derive(Serialize, Debug)]
/// Subset of the comment which was just inserted. This data is needed to populate the frontend
/// without calling for a complete refresh.
pub struct InsertedComment {
    /// Primary key.
    id: i32,
    /// Parent comment.
    parent: Option<i32>,
    /// Commentors details.
    author: Option<String>,
}

impl InsertedComment {
    /// Creates a new nested comment from a PrintedComment and a set of precalculated NestedComment children.
    fn new(comment: &PrintedComment) -> InsertedComment {
        let author = get_author(&comment.author, &comment.email, &comment.url);
        InsertedComment {
            id: comment.id,
            parent: comment.parent,
            author,
        }
    }
}

#[derive(Serialize, Debug)]
/// Subset of the comment which was just edited. This data is needed to populate the frontend
/// without calling for a complete refresh.
pub struct CommentEdits {
    /// Primary key.
    id: i32,
    /// Commentors details.
    author: Option<String>,
    /// Actual comment.
    text: String,
    /// Commentors indentifier.
    hash: String,
}

impl CommentEdits {
    /// Creates a new nested comment from a PrintedComment and a set of precalculated NestedComment children.
    fn new(comment: &PrintedComment) -> CommentEdits {
        let author = get_author(&comment.author, &comment.email, &comment.url);
        CommentEdits {
            id: comment.id,
            author,
            text: comment.text.to_owned(),
            hash: comment.hash.to_owned(),
        }
    }
}

#[derive(Serialize, Debug)]
/// Subset of the comments table which is to be nested and sent to the frontend.
pub struct NestedComment {
    /// Primary key.
    id: i32,
    /// Actual comment.
    text: String,
    /// Commentors author if given.
    author: Option<String>,
    /// Commentors indentifier.
    hash: String,
    /// Timestamp of creation.
    created: DateTime<Utc>,
    /// Comment children.
    children: Vec<NestedComment>,
    /// Total number of votes.
    votes: i32,
}

impl NestedComment {
    /// Creates a new nested comment from a PrintedComment and a set of precalculated NestedComment children.
    fn new(comment: &PrintedComment, children: Vec<NestedComment>) -> NestedComment {
        let date_time = DateTime::<Utc>::from_utc(comment.created, Utc);
        let author = get_author(&comment.author, &comment.email, &comment.url);
        let votes = count_votes(comment.likes, comment.dislikes);
        NestedComment {
            id: comment.id,
            text: comment.text.to_owned(),
            author,
            hash: comment.hash.to_owned(),
            created: date_time,
            children,
            votes,
        }
    }

    /// Returns a list of all comments, nested, for a given post denoted via the `path` variable.
    pub fn list(conn: &SqliteConnection, path: &str) -> Result<Vec<NestedComment>> {
        // Pull data from DB
        let comments = PrintedComment::list(conn, path)?;

        let mut graph = DiGraphMap::new();
        let mut top_level_ids = Vec::new();

        for comment in &comments {
            //For each comment, build a graph of parents and children
            graph.add_node(comment.id);

            //Generate edges if a relationship is found, stash as a root if not
            if let Some(parent_id) = comment.parent {
                graph.add_node(parent_id);
                graph.add_edge(parent_id, comment.id, ());
            } else {
                top_level_ids.push(comment.id);
            }
        }

        //Run over all root comments, recursively filling their children as we go
        let tree: Vec<_> = top_level_ids
            .into_iter()
            .map(|id| build_tree(&graph, id, &comments))
            .collect();

        Ok(tree)
    }
}

/// Construct a nested comment tree from the flat indexed data obtained from the database.
fn build_tree(graph: &DiGraphMap<i32, ()>, id: i32, comments: &[PrintedComment]) -> NestedComment {
    let children: Vec<NestedComment> = graph
        .neighbors(id)
        .map(|child_id| build_tree(graph, child_id, comments))
        .collect();

    //We can just unwrap here since the id value is always populated from a map over contents.
    let idx: usize = comments.iter().position(|c| c.id == id).unwrap();

    if !children.is_empty() {
        NestedComment::new(&comments[idx], children)
    } else {
        NestedComment::new(&comments[idx], Vec::new())
    }
}

/// Generates a value for author depending on the completeness of the author profile.
fn get_author(
    author: &Option<String>,
    email: &Option<String>,
    url: &Option<String>,
) -> Option<String> {
    if author.is_some() {
        author.to_owned()
    } else if email.is_some() {
        //We want to parse the email address to keep it somewhat confidential.
        let real_email = email.to_owned().unwrap();
        let at_index = real_email.find('@').unwrap_or_else(|| real_email.len());
        let (user, domain) = real_email.split_at(at_index);
        let first_dot = domain.find('.').unwrap_or_else(|| domain.len());
        let (_, trailing) = domain.split_at(first_dot);

        let mut email_obf = String::new();
        email_obf.push_str(user);
        email_obf.push_str("@****");
        email_obf.push_str(trailing);
        Some(email_obf)
    } else {
        //This can be something or nothing, since we don't need te parse it it doesn't matter
        url.to_owned()
    }
}

/// Calculates the total vote for a comment based on its likes and dislikes.
fn count_votes(likes: Option<i32>, dislikes: Option<i32>) -> i32 {
    likes.unwrap_or_else(|| 0) - dislikes.unwrap_or_else(|| 0)
}
